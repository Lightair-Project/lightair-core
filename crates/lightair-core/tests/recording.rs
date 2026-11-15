//! Which canvases Record records.

use lightair_core::{Canvas, Workspace, RecordingSettings};

#[test]
fn every_canvas_until_chosen_and_never_none() {
    let mut workspace = Workspace::new("Show");
    let landscape = workspace.primary_canvas().id;
    let vertical = Canvas::vertical();
    let vid = vertical.id;
    workspace.canvases.push(vertical);
    let fresh = RecordingSettings::default();
    assert_eq!(fresh.targets(&workspace), [landscape, vid], "every canvas by default");

    // An older workspace recorded one canvas: it still does.
    let older = RecordingSettings { canvas: Some(vid), ..RecordingSettings::default() };
    assert_eq!(older.targets(&workspace), [vid]);

    // Leaving canvases out, but never the last one.
    let only_vertical = fresh.with_canvas(&workspace, landscape, false);
    assert_eq!(only_vertical.targets(&workspace), [vid]);
    assert_eq!(only_vertical.with_canvas(&workspace, vid, false).targets(&workspace), [vid], "one stays");
    assert_eq!(only_vertical.with_canvas(&workspace, landscape, true).targets(&workspace), [landscape, vid]);

    // A removed canvas drops out.
    workspace.canvases.retain(|f| f.id != vid);
    assert_eq!(only_vertical.targets(&workspace), [landscape], "falls back to the primary");
}
