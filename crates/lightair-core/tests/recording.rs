//! Which canvases Record records.

use lightair_core::{Format, Project, RecordingSettings};

#[test]
fn every_canvas_until_chosen_and_never_none() {
    let mut project = Project::new("Show");
    let landscape = project.primary_format().id;
    let vertical = Format::vertical();
    let vid = vertical.id;
    project.formats.push(vertical);
    let fresh = RecordingSettings::default();
    assert_eq!(fresh.targets(&project), [landscape, vid], "every canvas by default");

    // An older project recorded one format: it still does.
    let older = RecordingSettings { format: Some(vid), ..RecordingSettings::default() };
    assert_eq!(older.targets(&project), [vid]);

    // Leaving canvases out, but never the last one.
    let only_vertical = fresh.with_format(&project, landscape, false);
    assert_eq!(only_vertical.targets(&project), [vid]);
    assert_eq!(only_vertical.with_format(&project, vid, false).targets(&project), [vid], "one stays");
    assert_eq!(only_vertical.with_format(&project, landscape, true).targets(&project), [landscape, vid]);

    // A removed format drops out.
    project.formats.retain(|f| f.id != vid);
    assert_eq!(only_vertical.targets(&project), [landscape], "falls back to the primary");
}
