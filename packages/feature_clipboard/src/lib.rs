use arboard::Clipboard;

/// Copies the given text to the system clipboard.
///
/// # Arguments
///
/// * `text` - A string slice that holds the text to be copied.
///
/// # Returns
///
/// * `Ok(())` if the text was copied successfully.
/// * `Err(String)` with an error message if copying failed.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    match Clipboard::new() {
        Ok(mut clipboard) => {
            if let Err(e) = clipboard.set_text(text) {
                let err_msg = format!("Failed to set clipboard text: {}", e);
                tracing::error!("{}", err_msg);
                Err(err_msg)
            } else {
                Ok(())
            }
        }
        Err(e) => {
            let err_msg = format!("Failed to initialize clipboard: {}", e);
            tracing::error!("{}", err_msg);
            Err(err_msg)
        }
    }
}