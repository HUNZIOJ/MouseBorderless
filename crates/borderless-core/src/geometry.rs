use crate::config::RemotePosition;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, width: i32, height: i32) -> Self {
        Self {
            left,
            top,
            width,
            height,
        }
    }

    pub fn right(self) -> i32 {
        self.left.saturating_add(self.width.saturating_sub(1))
    }

    pub fn bottom(self) -> i32 {
        self.top.saturating_add(self.height.saturating_sub(1))
    }

    pub fn is_valid(self) -> bool {
        self.width > 0
            && self.height > 0
            && self.left.checked_add(self.width - 1).is_some()
            && self.top.checked_add(self.height - 1).is_some()
    }

    /// Clamps a point into the rectangle. Invalid rectangles return the original point.
    pub fn clamp(self, point: Point) -> Point {
        self.try_clamp(point).unwrap_or(point)
    }

    pub fn try_clamp(self, point: Point) -> Option<Point> {
        self.is_valid().then(|| Point {
            x: point.x.clamp(self.left, self.right()),
            y: point.y.clamp(self.top, self.bottom()),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

pub fn detect_edge(point: Point, desktop: Rect, trigger_px: i32) -> Option<Edge> {
    if !desktop.is_valid() || trigger_px <= 0 {
        return None;
    }

    if point.x < desktop.left + trigger_px {
        Some(Edge::Left)
    } else if point.x > desktop.right() - trigger_px {
        Some(Edge::Right)
    } else if point.y < desktop.top + trigger_px {
        Some(Edge::Top)
    } else if point.y > desktop.bottom() - trigger_px {
        Some(Edge::Bottom)
    } else {
        None
    }
}

pub fn detect_edge_for_position(
    point: Point,
    desktop: Rect,
    trigger_px: i32,
    position: RemotePosition,
) -> Option<Edge> {
    if !desktop.is_valid() || trigger_px <= 0 {
        return None;
    }

    match position {
        RemotePosition::Left if point.x < desktop.left + trigger_px => Some(Edge::Left),
        RemotePosition::Right if point.x > desktop.right() - trigger_px => Some(Edge::Right),
        RemotePosition::Top if point.y < desktop.top + trigger_px => Some(Edge::Top),
        RemotePosition::Bottom if point.y > desktop.bottom() - trigger_px => Some(Edge::Bottom),
        _ => None,
    }
}

pub fn edge_for_position(position: RemotePosition) -> Edge {
    match position {
        RemotePosition::Left => Edge::Left,
        RemotePosition::Right => Edge::Right,
        RemotePosition::Top => Edge::Top,
        RemotePosition::Bottom => Edge::Bottom,
    }
}

pub fn opposite_edge(edge: Edge) -> Edge {
    match edge {
        Edge::Left => Edge::Right,
        Edge::Right => Edge::Left,
        Edge::Top => Edge::Bottom,
        Edge::Bottom => Edge::Top,
    }
}

/// Maps a point onto the remote entry edge. Invalid rectangles return the original local point.
pub fn map_entry_point(
    position: RemotePosition,
    local_point: Point,
    local: Rect,
    remote: Rect,
) -> Point {
    try_map_entry_point(position, local_point, local, remote).unwrap_or(local_point)
}

pub fn try_map_entry_point(
    position: RemotePosition,
    local_point: Point,
    local: Rect,
    remote: Rect,
) -> Option<Point> {
    if !local.is_valid() || !remote.is_valid() {
        return None;
    }

    Some(match position {
        RemotePosition::Left => Point::new(
            remote.right(),
            proportional(
                local_point.y,
                local.top,
                local.height,
                remote.top,
                remote.height,
            ),
        ),
        RemotePosition::Right => Point::new(
            remote.left,
            proportional(
                local_point.y,
                local.top,
                local.height,
                remote.top,
                remote.height,
            ),
        ),
        RemotePosition::Top => Point::new(
            proportional(
                local_point.x,
                local.left,
                local.width,
                remote.left,
                remote.width,
            ),
            remote.bottom(),
        ),
        RemotePosition::Bottom => Point::new(
            proportional(
                local_point.x,
                local.left,
                local.width,
                remote.left,
                remote.width,
            ),
            remote.top,
        ),
    })
}

fn proportional(
    value: i32,
    source_start: i32,
    source_len: i32,
    target_start: i32,
    target_len: i32,
) -> i32 {
    let source_offset = (value - source_start).clamp(0, source_len - 1) as i64;
    let numerator = source_offset * (target_len - 1) as i64;
    target_start + (numerator / (source_len - 1).max(1) as i64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemotePosition;

    #[test]
    fn maps_right_edge_y_ratio_to_remote_y() {
        let local = Rect::new(0, 0, 1920, 1080);
        let remote = Rect::new(0, 0, 2560, 1440);
        let point = map_entry_point(RemotePosition::Right, Point::new(1919, 540), local, remote);
        assert_eq!(point.x, 0);
        assert_eq!(point.y, 720);
    }

    #[test]
    fn detects_top_edge_inside_trigger_width() {
        let desktop = Rect::new(0, 0, 1920, 1080);
        assert_eq!(detect_edge(Point::new(500, 1), desktop, 2), Some(Edge::Top));
    }

    #[test]
    fn clamps_remote_coordinate_inside_desktop() {
        let desktop = Rect::new(100, 100, 800, 600);
        assert_eq!(desktop.clamp(Point::new(50, 999)), Point::new(100, 699));
    }

    #[test]
    fn invalid_rect_try_clamp_returns_none() {
        let desktop = Rect::new(100, 100, 0, 600);
        assert_eq!(desktop.try_clamp(Point::new(50, 999)), None);
    }

    #[test]
    fn invalid_rect_mapping_returns_none() {
        let local = Rect::new(0, 0, 1920, 0);
        let remote = Rect::new(0, 0, 2560, 1440);
        assert_eq!(
            try_map_entry_point(RemotePosition::Right, Point::new(1919, 540), local, remote),
            None
        );
    }

    #[test]
    fn detects_configured_top_edge_at_top_left_corner() {
        let desktop = Rect::new(0, 0, 1920, 1080);
        assert_eq!(
            detect_edge_for_position(Point::new(0, 0), desktop, 2, RemotePosition::Top),
            Some(Edge::Top)
        );
    }

    #[test]
    fn edge_serializes_snake_case() {
        #[derive(Serialize)]
        struct Wrapper {
            edge: Edge,
        }

        let serialized = toml::to_string(&Wrapper { edge: Edge::Top }).unwrap();
        assert_eq!(serialized, "edge = \"top\"\n");
    }
}
