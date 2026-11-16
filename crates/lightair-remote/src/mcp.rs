//! The Model Context Protocol (Streamable HTTP, JSON answers) at `/mcp`: Lightair as tools an
//! agent can call. Each tool is one of Lightair's methods with the same arguments; the
//! screenshot comes back as an image the agent can look at.

use serde_json::{Value, json};

use crate::http::Request;
use crate::rpc::{self, error};
use crate::Shared;

/// Protocol versions this server speaks (newest first).
const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Lightair is a live-production studio (like OBS). A workspace has scenes; \
each scene has objects (layers) drawn back to front on the canvas; most objects show a source \
(camera, display capture, image, text, browser, …) whose settings say what it shows; sounds have \
mixer tracks. The program scene is what's on air and recorded. Start with get_state to see \
scenes, objects, tracks and outputs; use screenshot to see the program. Scenes, objects, \
sources and tracks are named by id or by name. Positions and sizes are in canvas pixels \
(get_state lists canvases). Every change is undoable (undo).";

/// The bearer password, if one is asked.
pub(crate) fn authorized(req: &Request, shared: &Shared) -> bool {
    shared.password_ok(rpc::bearer(req))
}

/// One JSON-RPC message in: the answer (`None` for notifications and responses).
pub(crate) fn handle(body: &[u8], shared: &Shared) -> Option<String> {
    let msg: Value = match serde_json::from_slice(body) {
        Ok(m) => m,
        Err(e) => return Some(error(&Value::Null, rpc::PARSE, &e.to_string()).to_string()),
    };
    if let Value::Array(batch) = msg {
        // older clients may batch
        let out: Vec<Value> = batch.into_iter().filter_map(|m| one(m, shared)).collect();
        return (!out.is_empty()).then(|| Value::Array(out).to_string());
    }
    one(msg, shared).map(|v| v.to_string())
}

fn one(msg: Value, shared: &Shared) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        // a response to something we asked: we ask nothing
        return None;
    };
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result = match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(VERSIONS[0]);
            let version = VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(VERSIONS[0]);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "lightair", "title": "Lightair", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => Ok(call(&params, shared)),
        "resources/list" => Ok(json!({"resources": []})),
        "prompts/list" => Ok(json!({"prompts": []})),
        _ => Err((rpc::NO_METHOD, format!("no method {method}"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((c, m)) => error(&id, c, &m),
    })
}

/// A tool's call: its method's answer as text (or an image), or the error as a tool error.
fn call(params: &Value, shared: &Shared) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let mut args = params.get("arguments").cloned().unwrap_or(json!({}));
    if !args.is_object() {
        args = json!({});
    }
    let Some(method) = method_of(name, &mut args) else {
        return tool_error(&format!("Lightair has no tool named {name}"));
    };
    match shared.call("MCP", &method, args) {
        Ok(result) if method == "screenshot" => {
            let data = result.get("data").and_then(Value::as_str).unwrap_or_default();
            let size = format!(
                "Picture, {}×{}",
                result.get("width").and_then(Value::as_u64).unwrap_or(0),
                result.get("height").and_then(Value::as_u64).unwrap_or(0)
            );
            json!({"content": [
                {"type": "image", "data": data, "mimeType": result.get("mime").and_then(Value::as_str).unwrap_or("image/png")},
                {"type": "text", "text": size},
            ]})
        }
        Ok(result) => {
            let text = serde_json::to_string_pretty(&result).unwrap_or_default();
            let mut out = json!({"content": [{"type": "text", "text": text}]});
            if result.is_object() {
                out["structuredContent"] = result;
            }
            out
        }
        Err(e) => tool_error(&e.message),
    }
}

fn tool_error(message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

/// The Lightair method behind a tool (some take their method's last word as `action`).
fn method_of(tool: &str, args: &mut Value) -> Option<String> {
    let action = |args: &mut Value| args.as_object_mut()?.remove("action")?.as_str().map(str::to_owned);
    Some(match tool {
        "recording" => format!("record.{}", action(args)?),
        "streaming" => format!("stream.{}", action(args)?),
        "virtual_camera" => format!("vcam.{}", action(args)?),
        _ => TOOLS.iter().find(|t| t.name == tool)?.method.to_string(),
    })
}

struct Tool {
    name: &'static str,
    method: &'static str,
    title: &'static str,
    description: &'static str,
    /// Properties (JSON) and the required ones.
    properties: &'static str,
    required: &'static [&'static str],
    read_only: bool,
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "get_state",
        method: "state.get",
        title: "Get Lightair's state",
        description: "Scenes (and which is on air), each scene's objects with kind, position, size and visibility, sources, mixer tracks, canvases, and whether recording, streaming, the virtual camera and clips are on.",
        properties: "{}",
        required: &[],
        read_only: true,
    },
    Tool {
        name: "screenshot",
        method: "screenshot",
        title: "See the program",
        description: "A picture of what's on air (or of another scene). Use it to check how a change looks.",
        properties: r#"{"scene": {"type": "string", "description": "Scene id or name (default: the program)"}, "width": {"type": "integer", "description": "Picture width in pixels (default 960, at most the canvas's)"}}"#,
        required: &[],
        read_only: true,
    },
    Tool {
        name: "switch_scene",
        method: "scene.switch",
        title: "Put a scene on air",
        description: "Makes a scene the program (what viewers see), with the scene's transition.",
        properties: r#"{"scene": {"type": "string", "description": "Scene id or name"}}"#,
        required: &["scene"],
        read_only: false,
    },
    Tool {
        name: "create_scene",
        method: "scene.create",
        title: "Create a scene",
        description: "Adds an empty scene at the end of the list.",
        properties: r#"{"name": {"type": "string"}, "switch": {"type": "boolean", "description": "Also put it on air"}}"#,
        required: &["name"],
        read_only: false,
    },
    Tool {
        name: "rename_scene",
        method: "scene.rename",
        title: "Rename a scene",
        description: "Renames a scene.",
        properties: r#"{"scene": {"type": "string", "description": "Scene id or name"}, "name": {"type": "string"}}"#,
        required: &["scene", "name"],
        read_only: false,
    },
    Tool {
        name: "remove_scene",
        method: "scene.remove",
        title: "Remove a scene",
        description: "Removes a scene and its objects (undoable).",
        properties: r#"{"scene": {"type": "string", "description": "Scene id or name"}}"#,
        required: &["scene"],
        read_only: false,
    },
    Tool {
        name: "add_object",
        method: "object.add",
        title: "Add an object to a scene",
        description: "Adds an object in front of the others, centred and fitted to the canvas unless x/y/width/height are given. Kinds: text (settings.text), image (settings.file: a path), video (settings.local_file: a path; settings.looping), color (settings.color: 0xAARRGGBB number), browser (settings.url, width, height), display / window / app (settings.display / window / app: a value from list_capture_targets), camera (settings.device from list_capture_targets), microphone (settings.device), timer (settings.seconds), or any libobs source id. With `source`, shows an existing source again instead of making a new one.",
        properties: r#"{"kind": {"type": "string", "description": "text, image, video, color, browser, display, window, app, camera, microphone, timer, or a libobs source id"}, "scene": {"type": "string", "description": "Scene id or name (default: the scene being edited)"}, "name": {"type": "string"}, "settings": {"type": "object", "description": "The source's settings (see the kinds)"}, "source": {"type": "string", "description": "An existing source's id or name to show again (kind and settings are then ignored)"}, "x": {"type": "number"}, "y": {"type": "number"}, "width": {"type": "number"}, "height": {"type": "number"}}"#,
        required: &[],
        read_only: false,
    },
    Tool {
        name: "update_object",
        method: "object.update",
        title: "Change an object",
        description: "Changes an object's name, position, size, rotation, opacity, visibility or lock, and/or its source's settings (merged into the current ones). Only what's given changes.",
        properties: r#"{"object": {"type": "string", "description": "Object id or name"}, "scene": {"type": "string", "description": "Scene id or name (default: the scene being edited)"}, "name": {"type": "string"}, "visible": {"type": "boolean"}, "locked": {"type": "boolean"}, "x": {"type": "number"}, "y": {"type": "number"}, "width": {"type": "number"}, "height": {"type": "number"}, "rotation": {"type": "number", "description": "Degrees"}, "opacity": {"type": "number", "description": "0 to 1"}, "settings": {"type": "object", "description": "Source settings to merge"}}"#,
        required: &["object"],
        read_only: false,
    },
    Tool {
        name: "remove_object",
        method: "object.remove",
        title: "Remove an object",
        description: "Removes an object (and what's grouped under it) from a scene.",
        properties: r#"{"object": {"type": "string", "description": "Object id or name"}, "scene": {"type": "string", "description": "Scene id or name (default: the scene being edited)"}}"#,
        required: &["object"],
        read_only: false,
    },
    Tool {
        name: "move_object",
        method: "object.move",
        title: "Bring forward or send back",
        description: "Moves an object in its scene's stacking order.",
        properties: r#"{"object": {"type": "string", "description": "Object id or name"}, "scene": {"type": "string", "description": "Scene id or name (default: the scene being edited)"}, "position": {"type": "string", "enum": ["front", "back", "forward", "backward"]}}"#,
        required: &["object", "position"],
        read_only: false,
    },
    Tool {
        name: "get_source",
        method: "source.get",
        title: "Read a source's settings",
        description: "A source's kind and all its settings.",
        properties: r#"{"source": {"type": "string", "description": "Source id or name"}}"#,
        required: &["source"],
        read_only: true,
    },
    Tool {
        name: "list_capture_targets",
        method: "capture.targets",
        title: "List displays, windows, apps, cameras or microphones",
        description: "What add_object can capture: each target's title and the value to put in its settings.",
        properties: r#"{"kind": {"type": "string", "enum": ["display", "window", "app", "camera", "microphone"]}}"#,
        required: &["kind"],
        read_only: true,
    },
    Tool {
        name: "set_audio",
        method: "audio.set",
        title: "Set a track's volume or mute",
        description: "Sets a mixer track's volume (dB, 0 is unchanged, -60 nearly silent) and/or mutes it.",
        properties: r#"{"track": {"type": "string", "description": "Track id or name, or its source's"}, "volume_db": {"type": "number"}, "muted": {"type": "boolean"}}"#,
        required: &["track"],
        read_only: false,
    },
    Tool {
        name: "recording",
        method: "",
        title: "Record",
        description: "Starts, stops, pauses or resumes recording the program, adds a chapter marker, splits the file, or tells whether it's recording.",
        properties: r#"{"action": {"type": "string", "enum": ["start", "stop", "pause", "resume", "marker", "split", "status"]}}"#,
        required: &["action"],
        read_only: false,
    },
    Tool {
        name: "streaming",
        method: "",
        title: "Go live",
        description: "Starts or ends the stream to the workspace's destinations, or tells whether it's live.",
        properties: r#"{"action": {"type": "string", "enum": ["start", "stop", "status"]}}"#,
        required: &["action"],
        read_only: false,
    },
    Tool {
        name: "virtual_camera",
        method: "",
        title: "Virtual camera",
        description: "Starts or stops the virtual camera other apps see, or tells whether it's on.",
        properties: r#"{"action": {"type": "string", "enum": ["start", "stop", "status"]}}"#,
        required: &["action"],
        read_only: false,
    },
    Tool {
        name: "save_clip",
        method: "clip.save",
        title: "Save a clip",
        description: "Saves the last moments (the replay buffer) as a clip.",
        properties: "{}",
        required: &[],
        read_only: false,
    },
    Tool {
        name: "undo",
        method: "history.undo",
        title: "Undo",
        description: "Undoes the last change (made anywhere: the app, a remote client, an agent).",
        properties: "{}",
        required: &[],
        read_only: false,
    },
    Tool {
        name: "redo",
        method: "history.redo",
        title: "Redo",
        description: "Redoes the change last undone.",
        properties: "{}",
        required: &[],
        read_only: false,
    },
    Tool {
        name: "get_stats",
        method: "stats.get",
        title: "Performance",
        description: "Frame rate, render time, frames lagged, CPU and memory.",
        properties: "{}",
        required: &[],
        read_only: true,
    },
    Tool {
        name: "get_workspace",
        method: "workspace.get",
        title: "The whole workspace",
        description: "The workspace document as JSON: every scene, object, layout per canvas, source with its settings and effects, track, bus and output setting. For changes get_state and the other tools don't cover, with run_commands.",
        properties: "{}",
        required: &[],
        read_only: true,
    },
    Tool {
        name: "run_commands",
        method: "commands.execute",
        title: "Run document commands",
        description: "Applies Lightair commands (as in the workspace's journal: {\"SetTrackVolume\": {\"track\": id, \"volume_db\": -6}}, {\"AddEffect\": …}, …) as one undoable change. For anything the other tools don't cover; get_workspace shows the ids.",
        properties: r#"{"commands": {"type": "array", "items": {"type": "object"}}, "label": {"type": "string", "description": "What Undo calls it"}}"#,
        required: &["commands"],
        read_only: false,
    },
    Tool {
        name: "list_automations",
        method: "automation.list",
        title: "Automations",
        description: "Every automation (as JSON), the ready-made recipes, the global variables and the latest runs with what each step did.",
        properties: "{}",
        required: &[],
        read_only: true,
    },
    Tool {
        name: "set_automation",
        method: "automation.set",
        title: "Add or change an automation",
        description: "Saves an automation (replacing the one with its id; without an id, a new one). Shape: {\"name\", \"enabled\", \"trigger\": {\"on\": \"event\", \"kinds\": [\"follow\"|\"sub\"|\"paid\"|\"cheer\"|\"raid\"], \"min_amount\"} | {\"on\": \"command\", \"name\": \"!so\", \"who\": \"everyone\"|\"subscribers\"|\"vips\"|\"moderators\"|\"broadcaster\"} | {\"on\": \"chat\", \"contains\", \"first_time\"} | {\"on\": \"timer\", \"minutes\", \"min_messages\"} | {\"on\": \"manual\"}, \"conditions\": [{\"left\": \"{{ value }}\", \"op\": \">=\", \"right\": \"10\"}], \"steps\": [{\"do\": \"chat\", \"text\"} | {\"do\": \"call\", \"method\": a Lightair method like scene.switch, \"params\"} | {\"do\": \"speak\", \"text\"} | {\"do\": \"sound\", \"file\"} | {\"do\": \"wait\", \"seconds\"} | {\"do\": \"var\", \"name\", \"op\": \"set\"|\"add\"|\"push\"|\"clear\", \"value\", \"user\"} | {\"do\": \"points\", \"amount\"} | {\"do\": \"if\", \"when\": [conditions], \"then\": [steps], \"else\": [steps]} | {\"do\": \"random\", \"choices\": [[steps]]} | {\"do\": \"run\", \"automation\"} | {\"do\": \"http\", \"url\", \"method\", \"body\"} | {\"do\": \"stop\"}], \"queue\": \"\" or a name (runs in a queue wait their turn), \"cooldown\", \"user_cooldown\" (seconds)}. Text is a template: {{ name }}, {{ message }}, {{ amount }}, {{ value }}, {{ viewers }}, {{ platform }}, {{ args.0 }}, {{ target }}, {{ vars.x }}, {{ user_vars.x }}, {{ viewer.points }}, {{ random 1 6 }}, with filters | upper | lower | strip_at | default: x. Or {\"recipe\": key} to start from a recipe.",
        properties: r#"{"automation": {"type": "object", "description": "The automation (see the description)"}, "recipe": {"type": "string", "description": "A recipe's key, to add it as it is"}}"#,
        required: &[],
        read_only: false,
    },
    Tool {
        name: "remove_automation",
        method: "automation.remove",
        title: "Remove an automation",
        description: "Removes an automation.",
        properties: r#"{"automation": {"type": "string", "description": "Its id or name"}}"#,
        required: &["automation"],
        read_only: false,
    },
    Tool {
        name: "run_automation",
        method: "automation.run",
        title: "Run an automation",
        description: "Runs an automation now, as if its trigger had happened (with `args` as a command's words).",
        properties: r#"{"automation": {"type": "string", "description": "Its id or name"}, "args": {"type": "array", "items": {"type": "string"}}}"#,
        required: &["automation"],
        read_only: false,
    },
    Tool {
        name: "read_chat",
        method: "chat.history",
        title: "Read chat",
        description: "The latest chat messages and events (follows, subs, donations, raids) from every connected platform, oldest first.",
        properties: r#"{"limit": {"type": "integer", "description": "How many (default 50)"}}"#,
        required: &[],
        read_only: true,
    },
    Tool {
        name: "send_chat",
        method: "chat.send",
        title: "Send a chat message",
        description: "Sends a message to a platform's chat (a destination's channel), where sending is set up.",
        properties: r#"{"text": {"type": "string"}, "platform": {"type": "string", "description": "twitch, youtube, kick, … (default: every connected chat)"}}"#,
        required: &["text"],
        read_only: false,
    },
    Tool {
        name: "test_event",
        method: "widgets.test",
        title: "Send a test event",
        description: "A made-up follow, sub, donation, cheer, raid or chat message, to try widgets and automations.",
        properties: r#"{"kind": {"type": "string", "description": "follow, sub, paid, cheer, raid or message"}, "platform": {"type": "string", "description": "default twitch"}}"#,
        required: &["kind"],
        read_only: false,
    },
];

fn tools() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|t| {
            let properties: Value = serde_json::from_str(t.properties).unwrap_or(json!({}));
            json!({
                "name": t.name,
                "title": t.title,
                "description": t.description,
                "inputSchema": {"type": "object", "properties": properties, "required": t.required},
                "annotations": {"title": t.title, "readOnlyHint": t.read_only, "destructiveHint": false, "openWorldHint": false},
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn tool_names() -> Vec<&'static str> {
    TOOLS.iter().map(|t| t.name).collect()
}

#[cfg(test)]
pub(crate) fn schemas_parse() -> bool {
    TOOLS.iter().all(|t| serde_json::from_str::<Value>(t.properties).is_ok_and(|v| v.is_object()))
}

/// Answers what can be answered without Lightair running (`initialize`, `tools/list`, `ping`);
/// tool calls get an error saying it isn't. For `lightair mcp` while the app is closed.
pub fn answer_without_app(body: &[u8]) -> Option<String> {
    let shared = Shared {
        config: crate::Config { port: 0, lan: false, password: None },
        handler: std::sync::Arc::new(|_| Err(crate::ApiError::failed("Lightair isn't running (open it, and turn on Settings › Remote API)"))),
        stop: Default::default(),
        clients: Default::default(),
        next: Default::default(),
    };
    handle(body, &shared)
}
