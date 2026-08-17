mod move_follow;
mod move_window_to_display;
mod move_workspace_to_display;
mod placement;
mod rehome;
mod switch_workspace;
mod switch_workspace_both;

use rift_client::DisplayData;

use crate::{Result, state_error, usage_error};

pub use move_follow::move_follow;
pub use move_window_to_display::move_window_to_display;
pub use move_workspace_to_display::move_workspace_to_display;
pub use rehome::rehome;
pub use switch_workspace::switch_workspace;
pub use switch_workspace_both::switch_workspace_both;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayDirection {
    Next,
    Previous,
}

impl DisplayDirection {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "next" => Ok(Self::Next),
            "prev" | "previous" => Ok(Self::Previous),
            _ => Err(usage_error(format!(
                "invalid display direction: {value}; expected next or prev"
            ))),
        }
    }
}

fn adjacent_display(
    displays: &[DisplayData],
    source_uuid: &str,
    direction: DisplayDirection,
) -> Result<Option<DisplayData>> {
    if displays.len() < 2 {
        return Ok(None);
    }
    let source_index = displays
        .iter()
        .position(|display| display.uuid == source_uuid)
        .ok_or_else(|| state_error("source display disappeared"))?;
    let target_index = match direction {
        DisplayDirection::Next => (source_index + 1) % displays.len(),
        DisplayDirection::Previous => (source_index + displays.len() - 1) % displays.len(),
    };
    Ok(Some(displays[target_index].clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_client::{Point, Rect, Size};

    fn display(uuid: &str) -> DisplayData {
        DisplayData {
            uuid: uuid.into(),
            name: None,
            screen_id: 0,
            frame: Rect {
                origin: Point { x: 0.0, y: 0.0 },
                size: Size {
                    width: 100.0,
                    height: 100.0,
                },
            },
            space: Some(1),
            is_active_space: true,
            is_active_context: false,
            active_space_ids: vec![1],
            inactive_space_ids: Vec::new(),
        }
    }

    #[test]
    fn adjacent_display_wraps_in_both_directions() {
        let displays = vec![display("a"), display("b"), display("c")];
        assert_eq!(
            adjacent_display(&displays, "c", DisplayDirection::Next)
                .unwrap()
                .unwrap()
                .uuid,
            "a"
        );
        assert_eq!(
            adjacent_display(&displays, "a", DisplayDirection::Previous)
                .unwrap()
                .unwrap()
                .uuid,
            "c"
        );
    }
}
