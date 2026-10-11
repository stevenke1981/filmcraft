//! Plain FilmCraft title for this modified build; no upstream trademark artwork is embedded.

/// Draw the wordmark `height` points tall with its left edge at `left_center` (vertically centred).
/// Returns the drawn rect.
pub fn paint_wordmark(ui: &egui::Ui, left_center: egui::Pos2, height: f32, _dark: bool) -> Option<egui::Rect> {
    Some(ui.painter().text(left_center, egui::Align2::LEFT_CENTER, "FilmCraft", egui::FontId::proportional(height), ui.visuals().text_color()))
}
