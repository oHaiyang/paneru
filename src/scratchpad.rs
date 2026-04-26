use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::query::{Added, Has, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, NonSendMut, Query, Res, ResMut};
use bevy::math::IRect;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use tracing::debug;

use crate::commands::{Command, Direction, Operation, ScratchpadAction};
use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, FocusedMarker, Position, RepositionMarker, ResizeMarker,
    focus_entity, reposition_entity, reshuffle_around, resize_entity,
};
use crate::events::Event;
use crate::manager::{Origin, Size, Window};
use crate::overlay::ScratchpadOverlayManager;

const SCRATCHPAD_WIDTH_RATIO: f64 = 0.80;
const SCRATCHPAD_HEIGHT_RATIO: f64 = 0.70;
const SCRATCHPAD_GAP: i32 = 12;
const HIDDEN_OFFSET: i32 = 64;
const HIDDEN_SIZE: i32 = 1;

type ScratchpadWindowPlacementQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Position,
        &'static Bounds,
        Option<&'static RepositionMarker>,
        Option<&'static ResizeMarker>,
    ),
    With<ScratchpadWindowMarker>,
>;

#[derive(Component)]
pub struct ScratchpadWindowMarker;

#[derive(Default, Resource)]
pub struct ScratchpadState {
    visible: bool,
    windows: Vec<Entity>,
    last_focused: Option<Entity>,
}

impl ScratchpadState {
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    #[cfg(test)]
    pub fn contains(&self, entity: Entity) -> bool {
        self.windows.contains(&entity)
    }

    #[cfg(test)]
    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    fn show(&mut self) {
        if !self.windows.is_empty() {
            self.visible = true;
        }
    }

    fn hide(&mut self) {
        self.visible = false;
    }

    fn toggle(&mut self) {
        if self.visible {
            self.hide();
        } else {
            self.show();
        }
    }

    fn add_window(&mut self, entity: Entity) {
        self.windows.retain(|candidate| *candidate != entity);
        self.windows.push(entity);
        self.last_focused = Some(entity);
        self.show();
    }

    fn remove_window(&mut self, entity: Entity) {
        self.windows.retain(|candidate| *candidate != entity);
        if self.last_focused == Some(entity) {
            self.last_focused = self.windows.last().copied();
        }
        if self.windows.is_empty() {
            self.hide();
        }
    }

    fn focus_candidate(&self) -> Option<Entity> {
        self.last_focused.or_else(|| self.windows.last().copied())
    }

    fn raise_order(&self) -> Vec<Entity> {
        let focus = self.focus_candidate();
        self.windows
            .iter()
            .copied()
            .filter(|entity| Some(*entity) != focus)
            .chain(focus)
            .collect()
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn scratchpad_command_handler(
    mut messages: bevy::ecs::message::MessageReader<Event>,
    mut state: ResMut<ScratchpadState>,
    windows: Windows,
    mut workspaces: Query<(Entity, &mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::Command { command } = event else {
            continue;
        };

        match command {
            Command::Scratchpad(action) => {
                handle_scratchpad_action(action, &mut state, &mut workspaces, &mut commands);
            }
            Command::Window(Operation::Focus(direction)) => {
                focus_scratchpad_window(direction, &windows, &mut state, &mut commands);
            }
            Command::Window(Operation::Swap(direction)) => {
                swap_scratchpad_window(direction, &windows, &mut state, &mut commands);
            }
            Command::Window(Operation::Scratchpad) => {
                toggle_focused_window(&windows, &mut state, &mut workspaces, &mut commands);
            }
            _ => {}
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn hide_scratchpad_on_virtual_workspace_change(
    changed_active_workspace: Query<Entity, Added<ActiveWorkspaceMarker>>,
    mut state: ResMut<ScratchpadState>,
) {
    if state.visible && !changed_active_workspace.is_empty() {
        state.hide();
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn prune_scratchpad_windows(
    live_windows: Query<(), With<Window>>,
    mut state: ResMut<ScratchpadState>,
) {
    let before = state.windows.len();
    state
        .windows
        .retain(|entity| live_windows.get(*entity).is_ok());
    if state.windows.len() != before {
        state.last_focused = state
            .last_focused
            .filter(|entity| state.windows.contains(entity))
            .or_else(|| state.windows.last().copied());
    }
    if state.windows.is_empty() {
        state.hide();
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn position_scratchpad_windows(
    state: Res<ScratchpadState>,
    scratchpad_windows: ScratchpadWindowPlacementQuery,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    let viewport = active_display
        .display()
        .actual_display_bounds(active_display.dock(), &config);

    let targets = if state.visible {
        scratchpad_window_frames(scratchpad_bounds(viewport), state.windows.len())
    } else {
        vec![hidden_scratchpad_frame(viewport); state.windows.len()]
    };

    for (entity, target) in state.windows.iter().copied().zip(targets) {
        let Ok((_, position, bounds, reposition, resize)) = scratchpad_windows.get(entity) else {
            continue;
        };

        let target_origin = target.min;
        let target_size = target.size();
        if reposition.is_none_or(|marker| **marker != target_origin) && position.0 != target_origin
        {
            reposition_entity(entity, target_origin, &mut commands);
        }
        if resize.is_none_or(|marker| **marker != target_size) && bounds.0 != target_size {
            resize_entity(entity, target_size, &mut commands);
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn remember_focused_scratchpad_window(
    focused: Query<Entity, (Added<FocusedMarker>, With<ScratchpadWindowMarker>)>,
    mut state: ResMut<ScratchpadState>,
) {
    for entity in focused {
        if state.windows.contains(&entity) {
            state.last_focused = Some(entity);
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn update_scratchpad_overlay(
    state: Res<ScratchpadState>,
    active_display: ActiveDisplay,
    config: Res<Config>,
    overlay: Option<NonSendMut<ScratchpadOverlayManager>>,
) {
    let Some(mut overlay) = overlay else {
        return;
    };

    if !state.visible || state.windows.is_empty() {
        overlay.remove();
        return;
    }

    let viewport = active_display
        .display()
        .actual_display_bounds(active_display.dock(), &config);
    overlay.update(Some(nsrect_from_irect(scratchpad_bounds(viewport))));
}

fn handle_scratchpad_action(
    action: &ScratchpadAction,
    state: &mut ScratchpadState,
    workspaces: &mut Query<(Entity, &mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    commands: &mut Commands,
) {
    match action {
        ScratchpadAction::Toggle => state.toggle(),
        ScratchpadAction::Show => state.show(),
        ScratchpadAction::Hide => state.hide(),
    }

    if state.visible {
        for entity in state.raise_order() {
            focus_entity(entity, true, commands);
        }
    } else {
        focus_active_workspace(workspaces, commands);
    }
}

fn toggle_focused_window(
    windows: &Windows,
    state: &mut ScratchpadState,
    workspaces: &mut Query<(Entity, &mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    commands: &mut Commands,
) {
    let Some((_, focused_entity)) = windows.focused() else {
        return;
    };

    if state.windows.contains(&focused_entity) {
        state.remove_window(focused_entity);
        if let Ok(mut entity_commands) = commands.get_entity(focused_entity) {
            entity_commands.try_remove::<ScratchpadWindowMarker>();
        }

        for (_, mut strip, active) in workspaces.iter_mut() {
            if active {
                strip.append(focused_entity);
                break;
            }
        }

        if state.is_visible() {
            if let Some(entity) = state.focus_candidate() {
                focus_entity(entity, true, commands);
            }
        } else {
            focus_entity(focused_entity, true, commands);
        }
        reshuffle_around(focused_entity, commands);
        debug!("Moved {focused_entity} out of scratchpad");
        return;
    }

    let source_neighbour = workspaces
        .iter()
        .find_map(|(_, strip, _)| {
            strip.contains(focused_entity).then(|| {
                strip
                    .left_neighbour(focused_entity)
                    .or_else(|| strip.right_neighbour(focused_entity))
            })
        })
        .flatten();

    for (_, mut strip, _) in workspaces.iter_mut() {
        strip.remove(focused_entity);
    }

    state.add_window(focused_entity);
    commands
        .entity(focused_entity)
        .try_insert(ScratchpadWindowMarker);
    focus_entity(focused_entity, true, commands);

    if let Some(neighbour) = source_neighbour {
        reshuffle_around(neighbour, commands);
    }
    debug!("Moved {focused_entity} into scratchpad");
}

fn focus_scratchpad_window(
    direction: &Direction,
    windows: &Windows,
    state: &mut ScratchpadState,
    commands: &mut Commands,
) {
    let Some((_, index)) = focused_scratchpad_index(windows, state) else {
        return;
    };
    let Some(target_index) = scratchpad_target_index(direction, index, state.windows.len()) else {
        return;
    };
    let Some(entity) = state.windows.get(target_index).copied() else {
        return;
    };

    state.last_focused = Some(entity);
    focus_entity(entity, true, commands);
}

fn swap_scratchpad_window(
    direction: &Direction,
    windows: &Windows,
    state: &mut ScratchpadState,
    commands: &mut Commands,
) {
    let Some((focused_entity, index)) = focused_scratchpad_index(windows, state) else {
        return;
    };
    let Some(target_index) = scratchpad_target_index(direction, index, state.windows.len()) else {
        return;
    };
    if index == target_index {
        return;
    }

    if index < target_index {
        for idx in index..target_index {
            state.windows.swap(idx, idx + 1);
        }
    } else {
        for idx in (target_index..index).rev() {
            state.windows.swap(idx, idx + 1);
        }
    }

    state.last_focused = Some(focused_entity);
    focus_entity(focused_entity, true, commands);
}

fn focused_scratchpad_index(windows: &Windows, state: &ScratchpadState) -> Option<(Entity, usize)> {
    if !state.visible {
        return None;
    }
    let (_, focused_entity) = windows.focused()?;
    let index = state
        .windows
        .iter()
        .position(|entity| *entity == focused_entity)?;
    Some((focused_entity, index))
}

fn scratchpad_target_index(direction: &Direction, index: usize, len: usize) -> Option<usize> {
    match direction {
        Direction::West => index.checked_sub(1),
        Direction::East => (index + 1 < len).then_some(index + 1),
        Direction::First => Some(0),
        Direction::Last => len.checked_sub(1),
        Direction::North | Direction::South => None,
    }
}

fn focus_active_workspace(
    workspaces: &mut Query<(Entity, &mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    commands: &mut Commands,
) {
    let focus = workspaces.iter_mut().find_map(|(_, strip, active)| {
        active
            .then(|| strip.first().ok().and_then(|column| column.top()))
            .flatten()
    });

    if let Some(entity) = focus {
        focus_entity(entity, true, commands);
    }
}

pub(crate) fn scratchpad_bounds(viewport: IRect) -> IRect {
    let width = (f64::from(viewport.width()) * SCRATCHPAD_WIDTH_RATIO).round() as i32;
    let height = (f64::from(viewport.height()) * SCRATCHPAD_HEIGHT_RATIO).round() as i32;
    let size = Size::new(width.max(1), height.max(1));
    let min = viewport.center() - size / 2;
    IRect::from_corners(min, min + size)
}

fn scratchpad_window_frames(bounds: IRect, count: usize) -> Vec<IRect> {
    if count == 0 {
        return vec![];
    }

    let count_i32 = i32::try_from(count).unwrap_or(i32::MAX).max(1);
    let total_gap = SCRATCHPAD_GAP * (count_i32 - 1);
    let width = ((bounds.width() - total_gap) / count_i32).max(1);
    let mut frames = Vec::with_capacity(count);

    for index in 0..count_i32 {
        let min_x = bounds.min.x + index * (width + SCRATCHPAD_GAP);
        let max_x = if index + 1 == count_i32 {
            bounds.max.x
        } else {
            min_x + width
        };
        frames.push(IRect::from_corners(
            Origin::new(min_x, bounds.min.y),
            Origin::new(max_x, bounds.max.y),
        ));
    }

    frames
}

fn hidden_scratchpad_frame(viewport: IRect) -> IRect {
    let min = viewport.max + Origin::splat(HIDDEN_OFFSET);
    IRect::from_corners(min, min + Size::splat(HIDDEN_SIZE))
}

fn nsrect_from_irect(rect: IRect) -> NSRect {
    NSRect::new(
        NSPoint::new(f64::from(rect.min.x), f64::from(rect.min.y)),
        NSSize::new(f64::from(rect.width()), f64::from(rect.height())),
    )
}

#[cfg(test)]
mod tests {
    use super::{scratchpad_bounds, scratchpad_window_frames};
    use bevy::math::{IRect, IVec2};

    #[test]
    fn scratchpad_bounds_are_centered_in_viewport() {
        let viewport = IRect::from_corners(IVec2::new(0, 20), IVec2::new(1024, 768));
        let bounds = scratchpad_bounds(viewport);

        assert_eq!(bounds.min, IVec2::new(103, 132));
        assert_eq!(bounds.size(), IVec2::new(819, 524));
    }

    #[test]
    fn scratchpad_windows_share_bounds_horizontally() {
        let bounds = IRect::from_corners(IVec2::new(103, 132), IVec2::new(922, 656));
        let frames = scratchpad_window_frames(bounds, 2);

        assert_eq!(frames[0].min, IVec2::new(103, 132));
        assert_eq!(frames[0].size(), IVec2::new(403, 524));
        assert_eq!(frames[1].min, IVec2::new(518, 132));
        assert_eq!(frames[1].size(), IVec2::new(404, 524));
    }
}
