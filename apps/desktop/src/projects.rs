//! Native folder picker for workspaces.

/// Opens the OS folder dialog and returns a UTF-8 path when the user picks one.
#[must_use]
pub fn pick_folder() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("Choose a project folder")
        .pick_folder()
        .and_then(|path| path.to_str().map(str::to_owned))
}
