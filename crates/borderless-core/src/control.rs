use crate::config::RemotePosition;
use crate::geometry::{
    detect_edge_for_position, edge_for_position, map_entry_point, opposite_edge, Edge, Point, Rect,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlMode {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlOutput {
    None,
    EnterRemote(Point),
    MoveRemote(Point),
    ReturnLocal(Point),
}

#[derive(Clone, Debug)]
pub struct ControlState {
    local_desktop: Rect,
    remote_desktop: Rect,
    remote_position: RemotePosition,
    edge_trigger_px: i32,
    mode: ControlMode,
    last_local_point: Point,
    remote_point: Point,
}

impl ControlState {
    pub fn new(
        local_desktop: Rect,
        remote_desktop: Rect,
        remote_position: RemotePosition,
        edge_trigger_px: i32,
    ) -> Self {
        Self {
            local_desktop,
            remote_desktop,
            remote_position,
            edge_trigger_px,
            mode: ControlMode::Local,
            last_local_point: Point::new(local_desktop.left, local_desktop.top),
            remote_point: Point::new(remote_desktop.left, remote_desktop.top),
        }
    }

    pub fn mode(&self) -> ControlMode {
        self.mode
    }

    pub fn observe_local_pointer(&mut self, point: Point) -> ControlOutput {
        self.last_local_point = point;

        if self.mode == ControlMode::Remote {
            return ControlOutput::None;
        }

        let target_edge = edge_for_position(self.remote_position.clone());
        if detect_edge_for_position(
            point,
            self.local_desktop,
            self.edge_trigger_px,
            self.remote_position.clone(),
        ) == Some(target_edge)
        {
            self.mode = ControlMode::Remote;
            self.remote_point = self.entry_point(
                self.remote_position.clone(),
                point,
                self.local_desktop,
                self.remote_desktop,
            );
            return ControlOutput::EnterRemote(self.remote_point);
        }

        ControlOutput::None
    }

    pub fn apply_remote_delta(&mut self, dx: i32, dy: i32) -> ControlOutput {
        if self.mode != ControlMode::Remote {
            return ControlOutput::None;
        }

        let next = Point::new(self.remote_point.x + dx, self.remote_point.y + dy);
        let return_edge = opposite_edge(edge_for_position(self.remote_position.clone()));
        if crossed_edge(next, self.remote_desktop, return_edge) {
            self.mode = ControlMode::Local;
            return ControlOutput::ReturnLocal(self.local_return_point());
        }

        self.remote_point = self.remote_desktop.clamp(next);
        ControlOutput::MoveRemote(self.remote_point)
    }

    fn local_return_point(&self) -> Point {
        match self.remote_position {
            RemotePosition::Left => Point::new(
                self.local_desktop.left + self.edge_trigger_px - 1,
                self.last_local_point.y,
            ),
            RemotePosition::Right => Point::new(
                self.local_desktop.right() - self.edge_trigger_px + 1,
                self.last_local_point.y,
            ),
            RemotePosition::Top => Point::new(
                self.last_local_point.x,
                self.local_desktop.top + self.edge_trigger_px - 1,
            ),
            RemotePosition::Bottom => Point::new(
                self.last_local_point.x,
                self.local_desktop.bottom() - self.edge_trigger_px + 1,
            ),
        }
    }

    fn entry_point(
        &self,
        position: RemotePosition,
        local_point: Point,
        local: Rect,
        remote: Rect,
    ) -> Point {
        let mapped = map_entry_point(position.clone(), local_point, local, remote);

        if !local.is_valid() || !remote.is_valid() {
            return mapped;
        }

        remote.clamp(match position {
            RemotePosition::Left => Point::new(
                mapped.x,
                scale_axis(local_point.y, local.top, local.height, remote.top, remote.height),
            ),
            RemotePosition::Right => Point::new(
                mapped.x,
                scale_axis(local_point.y, local.top, local.height, remote.top, remote.height),
            ),
            RemotePosition::Top => Point::new(
                scale_axis(local_point.x, local.left, local.width, remote.left, remote.width),
                mapped.y,
            ),
            RemotePosition::Bottom => Point::new(
                scale_axis(local_point.x, local.left, local.width, remote.left, remote.width),
                mapped.y,
            ),
        })
    }
}

fn crossed_edge(point: Point, desktop: Rect, edge: Edge) -> bool {
    match edge {
        Edge::Left => point.x < desktop.left,
        Edge::Right => point.x > desktop.right(),
        Edge::Top => point.y < desktop.top,
        Edge::Bottom => point.y > desktop.bottom(),
    }
}

fn scale_axis(
    value: i32,
    source_start: i32,
    source_len: i32,
    target_start: i32,
    target_len: i32,
) -> i32 {
    let source_offset = (value - source_start).clamp(0, source_len - 1) as i64;
    target_start + ((source_offset * target_len as i64) / source_len as i64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemotePosition;
    use crate::geometry::{Point, Rect};

    fn controller() -> ControlState {
        ControlState::new(
            Rect::new(0, 0, 1920, 1080),
            Rect::new(0, 0, 1280, 720),
            RemotePosition::Right,
            2,
        )
    }

    #[test]
    fn entering_remote_mode_maps_to_remote_edge() {
        let mut state = controller();
        let output = state.observe_local_pointer(Point::new(1919, 540));
        assert_eq!(output, ControlOutput::EnterRemote(Point::new(0, 360)));
        assert_eq!(state.mode(), ControlMode::Remote);
    }

    #[test]
    fn remote_delta_crossing_return_edge_returns_local_control() {
        let mut state = controller();
        state.observe_local_pointer(Point::new(1919, 540));
        let output = state.apply_remote_delta(-10, 0);
        assert_eq!(output, ControlOutput::ReturnLocal(Point::new(1918, 540)));
        assert_eq!(state.mode(), ControlMode::Local);
    }

    #[test]
    fn top_position_enters_remote_at_top_left_corner() {
        let mut state = ControlState::new(
            Rect::new(0, 0, 1920, 1080),
            Rect::new(0, 0, 1280, 720),
            RemotePosition::Top,
            2,
        );

        let output = state.observe_local_pointer(Point::new(0, 0));
        assert_eq!(output, ControlOutput::EnterRemote(Point::new(0, 719)));
        assert_eq!(state.mode(), ControlMode::Remote);
    }
}
