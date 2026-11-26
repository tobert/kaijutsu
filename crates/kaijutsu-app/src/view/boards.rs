//! Browse board activity independently of the active score context.

mod trail;

use bevy::prelude::*;
use crate::input::{Action, InputPhase, context::{ActiveInputContexts, InputContext}, events::ActionFired};
use crate::ui::screen::Screen;
use super::room::{RoomState, nav::Station};
use super::board_feed::{BoardFeed, BoardTopic, BoardPost, poll_board_feed, reset_board_feed};
pub(super) use trail::BoardLayer;
use trail::{Trail, step};

#[derive(Resource, Default)]
pub(super) struct BoardNav {
    trail: Trail,
    return_to: Screen,
    return_zoom: Option<Station>,
}

impl BoardNav {
    pub fn layer(&self) -> BoardLayer { self.trail.here.layer }
    pub fn board(&self) -> Option<&str> { self.trail.here.board.as_deref() }

    pub fn boards<'a>(&self, feed: &'a BoardFeed) -> Vec<&'a str> {
        let mut boards = Vec::new();
        for topic in feed.topics() {
            if !boards.contains(&topic.board()) {
                boards.push(topic.board());
            }
        }
        boards
    }

    pub fn topics<'a>(&self, feed: &'a BoardFeed) -> Vec<&'a BoardTopic> {
        feed.topics().iter().filter(|topic| Some(topic.board()) == self.board()).collect()
    }

    pub fn topic<'a>(&self, feed: &'a BoardFeed) -> Option<&'a BoardTopic> {
        feed.topics().iter().find(|topic| Some(topic.board()) == self.board()
            && Some(topic.id.as_str()) == self.trail.here.topic.as_deref())
    }

    pub fn post<'a>(&self, feed: &'a BoardFeed) -> Option<&'a BoardPost> {
        if feed.selected()?.id != self.topic(feed)?.id { return None; }
        self.trail.here.post.and_then(|number| feed.posts().iter().find(|p| p.number == number))
            .or_else(|| feed.posts().last())
    }

    pub fn guide<'a>(&self, feed: &'a BoardFeed) -> Option<&'a BoardTopic> {
        feed.topics().iter().find(|topic| matches!(topic.board(), "chomosuke/music-notes" | "chomosuke/memory"))
    }

    fn advance(&mut self, feed: &BoardFeed, delta: isize) {
        match self.layer() {
            BoardLayer::Boards => {
                self.trail.here.board = step(&self.boards(feed), self.board(), delta).map(str::to_owned);
            }
            BoardLayer::Topics => {
                let ids: Vec<_> = self.topics(feed).into_iter().map(|t| t.id.as_str()).collect();
                self.trail.here.topic = step(&ids, self.trail.here.topic.as_deref(), delta).map(str::to_owned);
            }
            BoardLayer::Posts => {
                let numbers: Vec<_> = feed.posts().iter().map(|p| p.number).collect();
                self.trail.here.post = step(&numbers, self.post(feed).map(|p| p.number), delta);
            }
        }
    }
}

#[derive(bevy::ecs::system::SystemParam)]
pub(crate) struct BoardReader<'w> {
    nav: Option<Res<'w, BoardNav>>,
    feed: Option<Res<'w, BoardFeed>>,
}

#[derive(serde::Serialize)]
pub(crate) struct BoardLocation<'a> {
    layer: BoardLayer,
    board: Option<&'a str>,
    topic_id: Option<&'a str>,
    topic_path: Option<&'a str>,
    post: Option<usize>,
    loading: bool,
    error: Option<&'a str>,
}

impl BoardReader<'_> {
    pub(crate) fn location(&self) -> Option<BoardLocation<'_>> {
        let nav = self.nav.as_deref()?;
        let feed = self.feed.as_deref()?;
        let topic = nav.topic(feed);
        Some(BoardLocation {
            layer: nav.layer(),
            board: nav.board(),
            topic_id: topic.map(|t| t.id.as_str()),
            topic_path: topic.map(|t| t.path.as_str()),
            post: nav.post(feed).map(|p| p.number),
            loading: feed.loading(),
            error: feed.error(),
        })
    }
}

pub struct BoardsPlugin;

impl Plugin for BoardsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<BoardNav>().init_resource::<BoardFeed>()
            .add_plugins(super::board_scene::BoardScenePlugin)
            .add_systems(Update, (
                poll_board_feed.run_if(in_state(Screen::Boards)),
                board_actions,
                reconcile.run_if(in_state(Screen::Boards)),
            ).chain().after(InputPhase::Dispatch))
            .add_systems(OnExit(Screen::Boards), reset_board_feed);
    }
}

fn reconcile(mut nav: ResMut<BoardNav>, mut feed: ResMut<BoardFeed>) {
    if feed.topics().is_empty() && feed.loading() { return; }
    if !feed.topics().iter().any(|t| Some(t.board()) == nav.board()) {
        let board = feed.topics().first().map(|t| t.board().to_owned());
        if nav.trail.here.board != board { nav.trail.here.board = board; }
    }
    if nav.topic(&feed).is_none() {
        let topic = nav.topics(&feed).first().map(|t| t.id.clone());
        if nav.trail.here.topic != topic {
            nav.trail.here.topic = topic;
            nav.trail.here.post = None;
        }
    }
    if let Some(id) = nav.trail.here.topic.as_deref()
        && feed.selected().is_none_or(|t| t.id != id)
    {
        feed.focus(id);
    }
    if nav.layer() == BoardLayer::Posts && let Some(number) = nav.post(&feed).map(|p| p.number)
        && nav.trail.here.post != Some(number)
    {
        nav.trail.here.post = Some(number);
    }
}

fn board_actions(
    mut events: MessageReader<ActionFired>, screen: Res<State<Screen>>, active: Res<ActiveInputContexts>,
    mut next: ResMut<NextState<Screen>>, mut nav: ResMut<BoardNav>, feed: Res<BoardFeed>,
    mut room: ResMut<RoomState>,
) {
    for event in events.read() {
        if event.action == Action::OpenBoards {
            if matches!(screen.get(), Screen::Conversation | Screen::Heroic | Screen::Room)
                && active.0.iter().any(|ctx| matches!(ctx, InputContext::Navigation | InputContext::Heroic
                    | InputContext::RoomNav | InputContext::WellZoomed))
                && !active.contains(InputContext::QuickContext)
            {
                nav.return_to = *screen.get();
                nav.return_zoom = room.zoomed;
                nav.trail = Trail::default();
                next.set(Screen::Boards);
            }
            continue;
        }
        if *screen.get() != Screen::Boards || event.context != InputContext::Boards { continue; }
        match event.action {
            Action::BoardPrevious => nav.advance(&feed, -1),
            Action::BoardNext => nav.advance(&feed, 1),
            Action::BoardConfirm => match nav.layer() {
                BoardLayer::Boards if nav.board().is_some() => nav.trail.here.layer = BoardLayer::Topics,
                BoardLayer::Topics if nav.topic(&feed).is_some() => nav.trail.here.layer = BoardLayer::Posts,
                _ => {}
            },
            Action::PopLevel => {
                if !nav.trail.back() {
                    if nav.return_to == Screen::Room { room.zoomed = nav.return_zoom.take(); }
                    next.set(nav.return_to);
                }
            }
            Action::BoardGuide => {
                if let Some(topic) = nav.guide(&feed) { nav.trail.visit_guide(topic.board(), &topic.id); }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_from_boards_restores_the_room_station_zoom() {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin).init_state::<Screen>()
            .add_message::<ActionFired>().init_resource::<BoardFeed>().init_resource::<BoardNav>()
            .init_resource::<RoomState>()
            .insert_resource(ActiveInputContexts(vec![InputContext::WellZoomed]))
            .add_systems(Update, board_actions);
        app.world_mut().resource_mut::<NextState<Screen>>().set(Screen::Room);
        app.update();
        app.world_mut().resource_mut::<RoomState>().zoomed = Some(Station::TimeWell);
        app.world_mut().write_message(ActionFired::new(Action::OpenBoards, InputContext::WellZoomed));
        app.update();
        app.update();
        assert_eq!(*app.world().resource::<State<Screen>>().get(), Screen::Boards);
        app.world_mut().resource_mut::<RoomState>().zoomed = None;
        app.world_mut().write_message(ActionFired::new(Action::PopLevel, InputContext::Boards));
        app.update();
        app.update();
        assert_eq!(*app.world().resource::<State<Screen>>().get(), Screen::Room);
        assert_eq!(app.world().resource::<RoomState>().zoomed, Some(Station::TimeWell));
    }
}
