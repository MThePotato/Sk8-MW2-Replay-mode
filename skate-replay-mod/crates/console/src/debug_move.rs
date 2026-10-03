use bevy::prelude::*;
use bevy::ui::{BackgroundGradient, BorderColor, BoxShadow, ColorStop, FocusPolicy, LinearGradient};
use net::{
    ClientActionInbox, LocalPresentClient, LookState, PresentedSnapshot, look_angles_from_degrees,
};
use sim::{ClientAction, ClientLifecycle, MatchPhase, SpawnPick};
use ui::UiLayer;

use crate::feature_dispatch::DebugPosOverlay;
use crate::replay_icons::{ReplayIcons, TINT_ACTIVE, TINT_CUT, TINT_IDLE, TINT_KEYFRAME, TINT_RAMP};
use crate::{
    ConsoleCommand, ConsoleDispatch, ConsoleLine, ConsoleRegistry, ConsoleSettings, ConsoleState,
    plugin::WaitMovePose,
};

#[derive(Component)]
pub(crate) struct ShowposHud;
#[derive(Component)]
pub(crate) struct SkateHud;
#[derive(Component)]
pub(crate) struct SkateTimeline;
#[derive(Component)]
pub(crate) struct TimelineNode(TimelineKind);

#[derive(Clone, Copy)]
enum TimelineKind {
    Playhead,
    Keyframe(usize),
    Ramp(usize),
    Segment(usize),
    Cut(usize),
    TrimIn,
    TrimOut,
    ShadeLeft,
    ShadeRight,
    /// Ring around the marker a hold-B delete would remove.
    Target,
}

#[derive(Component)]
pub(crate) struct TimelineTime;
#[derive(Component)]
pub(crate) struct CameraStrip;
#[derive(Component)]
pub(crate) struct CameraStripEntry(u8);
/// Name of the camera on screen, beside the camera strip.
#[derive(Component)]
pub(crate) struct CameraStripLabel;

/// Context button prompts under the timeline.
#[derive(Component)]
pub(crate) struct PromptBar;
#[derive(Component)]
pub(crate) struct PromptEntry(usize);
#[derive(Component)]
pub(crate) struct PromptChip(usize);
#[derive(Component, Clone, Copy)]
pub(crate) enum PromptPart {
    /// "Hold" / "Release" in front of the button.
    Prefix(usize),
    /// The button name inside the chip.
    Button(usize),
    /// What the button does.
    Action(usize),
}
const PROMPT_SLOTS: usize = 10;

/// Camera wheel (hold X): every camera around a ring.
#[derive(Component)]
pub(crate) struct CamWheel;
#[derive(Component)]
pub(crate) struct CamWheelEntry(u8);
#[derive(Component)]
pub(crate) struct CamWheelCenter;
const WHEEL_SIZE: f32 = 460.0;
const WHEEL_RADIUS: f32 = 180.0;

/// One-line explanation of the focused menu row.
#[derive(Component)]
pub(crate) struct MenuDescription;

/// Load-replay list: the saved clips, shown in place of the editor menu.
#[derive(Component)]
pub(crate) struct LoadPanel;
#[derive(Component)]
pub(crate) struct LoadRow(usize);
#[derive(Component)]
pub(crate) struct LoadName(usize);
#[derive(Component)]
pub(crate) struct LoadInfo(usize);
#[derive(Component)]
pub(crate) struct LoadCount;
const LOAD_ROWS: usize = 8;

const TIMELINE_WIDTH: f32 = 600.0;
const TIMELINE_HEIGHT: f32 = 22.0;
const TIMELINE_MARKERS: usize = 32;
const TIMELINE_RAMPS: usize = 16;
const TIMELINE_CUTS: usize = 16;

/// Dark glass used by every replay panel: a diagonal gradient over the game
/// plus a hairline border and soft drop shadow.
fn glass_background() -> BackgroundGradient {
    BackgroundGradient::from(LinearGradient::new(
        LinearGradient::TO_BOTTOM_RIGHT,
        vec![
            ColorStop::new(Color::srgba(0.11, 0.13, 0.17, 0.93), Val::Percent(0.0)),
            ColorStop::new(Color::srgba(0.04, 0.05, 0.07, 0.90), Val::Percent(55.0)),
            ColorStop::new(Color::srgba(0.015, 0.02, 0.03, 0.92), Val::Percent(100.0)),
        ],
    ))
}

fn panel_shadow() -> BoxShadow {
    BoxShadow::new(Color::srgba(0.0, 0.0, 0.0, 0.55), px(0), px(6), px(0), px(14))
}

fn hairline() -> BorderColor {
    BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.10))
}

fn cam_name(cam: u8) -> &'static str {
    // Keep in sync with render_anim's replay_cam_name.
    match cam {
        0 => "recorded",
        1 => "orbit",
        2 => "left",
        3 => "right",
        4 => "top",
        5 => "chase",
        6 => "free",
        7 => "keyframe",
        8 => "firstperson",
        9 => "fisheye",
        10 => "tripod",
        11 => "follow",
        12 => "body",
        _ => "board",
    }
}

fn cam_index(name: &str) -> Option<u8> {
    (0..14u8).find(|&c| cam_name(c) == name)
}

/// Maps a playback speed (0.125x..4x) to a green->red segment colour.
fn speed_color(speed: f32) -> Color {
    let t = ((speed - 0.125) / (4.0 - 0.125)).clamp(0.0, 1.0);
    Color::srgb(
        0.30 + (0.95 - 0.30) * t,
        0.85 + (0.30 - 0.85) * t,
        0.40 + (0.25 - 0.40) * t,
    )
}

pub(crate) fn current_skin_name() -> String {
    let names = assets::bot_model::skin_names();
    names
        .get(assets::bot_model::selected_skin())
        .cloned()
        .unwrap_or_else(|| "Default (Soldier)".to_owned())
}

/// The replay editor menu (Start / Tab), drawn from `frame::REPLAY_MENU`.
/// The skate adapter owns its logic; this side draws it and reports mouse
/// input.
#[derive(Component)]
pub(crate) struct ReplayMenu;
/// A menu row. Clicking activates it (or steps an adjustable row forward).
#[derive(Component)]
pub(crate) struct MenuRow(usize);
#[derive(Component)]
pub(crate) struct MenuValue(usize);
/// The < and > buttons on adjustable rows.
#[derive(Component)]
pub(crate) struct MenuArrow(usize, i8);
/// Toast / hold-to-confirm strip at the top of the replay column.
#[derive(Component)]
pub(crate) struct ReplayToast;
#[derive(Component)]
pub(crate) struct ReplayToastText;
/// The 9:16 TikTok guide frame drawn over a replay.
#[derive(Component)]
pub(crate) struct PortraitGuide;
/// The 2.39:1 cinematic letterbox bars drawn over a replay.
#[derive(Component)]
pub(crate) struct CinematicBars;
#[derive(Component)]
pub(crate) struct ReplayHoldTrack;
#[derive(Component)]
pub(crate) struct ReplayHoldBar;

const MENU_WIDTH: f32 = 300.0;
const HOLD_BAR_WIDTH: f32 = 220.0;

const REPLAY_HELP_PAD: &str = "PLAYBACK
A  play / pause           LS  scrub (catches on markers)
LT / RT  slow / fast      D-pad left / right  frame step
D-pad up / down  jump to the previous / next marker

CAMERAS
LB / RB  previous / next camera (cuts while playing)
X  cut here (tap again: next camera)
Hold X  camera wheel: point RS at one, release to cut
Y  keyframe from the view (on a keyframe: update it)
B on a marker  pick it up, move it, B / A put down
Hold B on a marker  delete it
Free / body / board cam:  RS click  camera control
    LS fly   RS look or aim   LT / RT down / up or dolly

QUICK EDIT (hold RB)
D-pad left / right  trim in / out   D-pad up  speed ramp
D-pad down  clear trim   LS  zoom (up / down), roll
X  9:16 frame   A  cinematic bars   B  undo   Y  redo

Start  editor menu   LS click  hide HUD   Hold Back  exit";

const REPLAY_HELP_KEYS: &str = "PLAYBACK
Space  play / pause       , and .  frame step
Left / Right  frame step   Up / Down or PgUp / PgDn  markers

CAMERAS
C  next camera (cuts while playing)   1 - 9, 0  cut to camera
X  cut   (tap again: next camera)
K  keyframe (on a keyframe: update it)
G  pick up / put down marker   Hold Backspace  delete
Mouse  click / drag the timeline, drag markers and trim lines
Free cam:  WASD fly   Q / E down / up   arrows look
Body / board cam:  arrows aim   mouse wheel dolly

EDIT
[ ]  trim in / out         \\  clear trim
V  speed ramp              - =  zoom    N M  roll
P  TikTok 9:16 frame       Ctrl+Z / Ctrl+Y  undo / redo

Tab  editor menu           H  hide HUD";

fn replay_text(font: &Handle<Font>, text: impl Into<String>, size: f32, color: Color) -> impl Bundle {
    (
        Text::new(text),
        TextFont { font: font.clone().into(), font_size: FontSize::Px(size), ..default() },
        TextColor(color),
    )
}

fn spawn_menu_arrow(parent: &mut ChildSpawnerCommands, font: &Handle<Font>, row: usize, dir: i8) {
    parent
        .spawn((
            Button,
            MenuArrow(row, dir),
            Node {
                width: px(18),
                height: px(18),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                border_radius: BorderRadius::all(px(3)),
                ..default()
            },
            BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.06)),
        ))
        .with_children(|b| {
            b.spawn(replay_text(font, if dir < 0 { "<" } else { ">" }, 12.0, Color::srgb(0.75, 0.80, 0.85)));
        });
}

pub(crate) fn spawn_showpos_hud(
    commands: &mut Commands,
    font: Handle<Font>,
    replay_font: Handle<Font>,
    icons: &ReplayIcons,
) {
    commands.insert_resource(TimelineIcons {
        keyframe: icons.keyframe.clone(),
        cut: icons.cut.clone(),
        ramp: icons.ramp.clone(),
    });
    // Skate status / replay help: top-right, which MW2's HUD leaves empty, so
    // it clears the scorebar and killfeed while skating and grows downward
    // away from the replay dock.
    commands.spawn((SkateHud, UiLayer::Overlay, Visibility::Hidden,
        Node { position_type: PositionType::Absolute, top:px(24), right:px(24),padding:UiRect::axes(px(14),px(10)), border: UiRect::all(px(1)), border_radius: BorderRadius::all(px(6)), ..default() },
        BackgroundColor(Color::srgba(0.02,0.03,0.04,0.7)),
        glass_background(),
        BorderColor::all(Color::srgba(1.0, 0.78, 0.25, 0.30)),
        panel_shadow(),
        GlobalZIndex(19000),Text::new(""),
        TextFont{font:replay_font.clone().into(),font_size:FontSize::Px(16.),..default()},TextColor(Color::srgb(0.93,0.95,0.97))));

    // Below the MW2 minimap, which scales with screen height.
    commands.spawn((
        ShowposHud,
        UiLayer::Overlay,
        Visibility::Hidden,
        Node {
            position_type: PositionType::Absolute,
            left: px(12),
            top: Val::Vh(25.0),
            padding: UiRect::all(px(6)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.02, 0.03, 0.04, 0.72)),
        GlobalZIndex(19_000),
        Text::new(""),
        TextFont {
            font: font.clone().into(),
            font_size: FontSize::Px(14.0),
            ..default()
        },
        TextColor(Color::srgb(0.95, 0.85, 0.40)),
    ));

    // TikTok guide: a centred 9:16 frame with everything outside it dimmed.
    // Renders crop to exactly this region in ffmpeg, so the outline is only
    // ever a framing aid and never reaches the video.
    commands
        .spawn((
            PortraitGuide,
            UiLayer::Overlay,
            Visibility::Hidden,
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                ..default()
            },
            FocusPolicy::Pass,
            GlobalZIndex(18_000),
        ))
        .with_children(|row| {
            let shade = || {
                (
                    Node {
                        flex_grow: 1.0,
                        flex_basis: px(0),
                        height: Val::Percent(100.0),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.30)),
                )
            };
            row.spawn(shade());
            row.spawn((
                Node {
                    height: Val::Percent(100.0),
                    aspect_ratio: Some(9.0 / 16.0),
                    flex_shrink: 0.0,
                    border: UiRect::all(px(2)),
                    ..default()
                },
                BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.85)),
            ))
            .with_children(|frame| {
                frame.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        bottom: px(10),
                        left: px(0),
                        width: Val::Percent(100.0),
                        justify_content: JustifyContent::Center,
                        ..default()
                    },
                    replay_text(&replay_font, "9:16  ·  1080x1920  ·  TikTok", 12.0, Color::srgba(1.0, 1.0, 1.0, 0.75)),
                ));
            });
            row.spawn(shade());
        });

    // Cinematic bars: a 2.39:1 band with pure black bars above and below.
    // Renders bake the same bars in, so what you see is what you get.
    commands
        .spawn((
            CinematicBars,
            UiLayer::Overlay,
            Visibility::Hidden,
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                ..default()
            },
            FocusPolicy::Pass,
            GlobalZIndex(18_000),
        ))
        .with_children(|col| {
            let bar = || {
                (
                    Node {
                        flex_grow: 1.0,
                        flex_basis: px(0),
                        width: Val::Percent(100.0),
                        ..default()
                    },
                    BackgroundColor(Color::BLACK),
                )
            };
            col.spawn(bar());
            col.spawn(Node {
                width: Val::Percent(100.0),
                aspect_ratio: Some(2.39),
                flex_shrink: 0.0,
                max_height: Val::Percent(100.0),
                ..default()
            });
            col.spawn(bar());
        });

    // The replay tools share one bottom-right dock: a flex row holding the
    // timeline column and the render panel, so they can't overlap each other
    // and stay clear of the console at bottom-left. All members share one
    // visibility rule, so a hidden member never leaves a gap.
    let dock = commands
        .spawn((
            UiLayer::Overlay,
            Node {
                position_type: PositionType::Absolute,
                right: px(24),
                bottom: px(24),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::FlexEnd,
                column_gap: px(16),
                ..default()
            },
            GlobalZIndex(19_500),
        ))
        .id();
    let column = commands
        .spawn((
            ChildOf(dock),
            Node {
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::FlexStart,
                row_gap: px(6),
                ..default()
            },
        ))
        .id();

    // Toasts and hold-to-confirm progress sit on top of the column. It is
    // display:none when idle, so it never leaves a gap.
    commands
        .spawn((
            ChildOf(column),
            ReplayToast,
            Visibility::Hidden,
            Node {
                display: Display::None,
                flex_direction: FlexDirection::Column,
                row_gap: px(4),
                padding: UiRect::axes(px(10), px(5)),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(5)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.03, 0.04, 0.85)),
            BorderColor::all(Color::srgba(1.0, 0.78, 0.25, 0.35)),
        ))
        .with_children(|toast| {
            toast.spawn((ReplayToastText, replay_text(&replay_font, "", 14.0, Color::srgb(0.95, 0.96, 0.98))));
            toast
                .spawn((
                    ReplayHoldTrack,
                    Node {
                        display: Display::None,
                        width: px(HOLD_BAR_WIDTH),
                        height: px(4),
                        border_radius: BorderRadius::all(px(2)),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.15)),
                ))
                .with_children(|track| {
                    track.spawn((
                        ReplayHoldBar,
                        Node {
                            width: px(0),
                            height: px(4),
                            border_radius: BorderRadius::all(px(2)),
                            ..default()
                        },
                        BackgroundColor(TINT_ACTIVE),
                    ));
                });
        });

    commands
        .spawn((
            ChildOf(column),
            CameraStrip,
            Visibility::Hidden,
            Node {
                flex_direction: FlexDirection::Row,
                column_gap: px(6),
                padding: UiRect::all(px(4)),
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.35)),
        ))
        .with_children(|strip| {
            for i in 0..14u8 {
                strip.spawn((
                    CameraStripEntry(i),
                    Node {
                        width: px(26),
                        height: px(22),
                        align_items: AlignItems::Center,
                        justify_content: JustifyContent::Center,
                        border_radius: BorderRadius::all(px(4)),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.04)),
                    ImageNode {
                        image: icons.cams[i as usize].clone(),
                        color: TINT_IDLE,
                        ..default()
                    },
                ));
            }
            strip.spawn((
                CameraStripLabel,
                Node {
                    align_self: AlignSelf::Center,
                    margin: UiRect::left(px(6)),
                    ..default()
                },
                replay_text(&replay_font, "", 13.0, Color::srgb(0.95, 0.85, 0.40)),
            ));
        });

    commands
        .spawn((
            ChildOf(column),
            SkateTimeline,
            Visibility::Hidden,
            Interaction::default(),
            bevy::ui::RelativeCursorPosition::default(),
            Node {
                width: px(TIMELINE_WIDTH),
                height: px(TIMELINE_HEIGHT),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
            BackgroundGradient::from(LinearGradient::new(
                LinearGradient::TO_BOTTOM_RIGHT,
                vec![
                    ColorStop::new(Color::srgba(0.09, 0.11, 0.14, 0.88), Val::Percent(0.0)),
                    ColorStop::new(Color::srgba(0.02, 0.025, 0.035, 0.88), Val::Percent(100.0)),
                ],
            )),
            hairline(),
            panel_shadow(),
        ))
        .with_children(|tl| {
            // Background speed-ramp segments (green->red). These and the
            // marker pools below grow on demand in `update_skate_timeline`.
            for i in 0..=TIMELINE_RAMPS {
                tl.spawn(segment_bundle(i));
            }
            // Trim dimming overlays (outside the active region).
            for shade in [TimelineKind::ShadeLeft, TimelineKind::ShadeRight] {
                tl.spawn((
                    TimelineNode(shade),
                    Visibility::Hidden,
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(0),
                        top: px(0),
                        width: px(0),
                        height: px(TIMELINE_HEIGHT),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
                ));
            }
            // Speed-ramp markers (orange dots), keyframes (amber diamonds)
            // and camera cuts (cyan flags along the top edge).
            for i in 0..TIMELINE_RAMPS {
                tl.spawn(ramp_bundle(i, &icons.ramp));
            }
            for i in 0..TIMELINE_MARKERS {
                tl.spawn(keyframe_bundle(i, &icons.keyframe));
            }
            for i in 0..TIMELINE_CUTS {
                tl.spawn(cut_bundle(i, &icons.cut));
            }
            // Ring around the marker a hold-B delete would remove.
            tl.spawn((
                TimelineNode(TimelineKind::Target),
                Visibility::Hidden,
                Node {
                    position_type: PositionType::Absolute,
                    left: px(0),
                    top: px(-3),
                    width: px(18),
                    height: px(TIMELINE_HEIGHT + 6.0),
                    border: UiRect::all(px(2)),
                    border_radius: BorderRadius::all(px(5)),
                    ..default()
                },
                BorderColor::all(Color::srgba(1.0, 0.45, 0.40, 0.95)),
                BoxShadow::new(Color::srgba(1.0, 0.35, 0.30, 0.45), px(0), px(0), px(1), px(6)),
                ZIndex(3),
            ));
            // Playhead on top of markers, with a soft glow so it reads
            // against both the track and the game behind it.
            tl.spawn((
                TimelineNode(TimelineKind::Playhead),
                Node {
                    position_type: PositionType::Absolute,
                    left: px(0),
                    top: px(0),
                    width: px(2),
                    height: px(TIMELINE_HEIGHT),
                    ..default()
                },
                BackgroundColor(Color::WHITE),
                BoxShadow::new(Color::srgba(1.0, 1.0, 1.0, 0.35), px(0), px(0), px(1), px(6)),
                ZIndex(5),
            ));
            tl.spawn((
                TimelineNode(TimelineKind::TrimIn),
                Visibility::Hidden,
                Node {
                    position_type: PositionType::Absolute,
                    left: px(0),
                    top: px(0),
                    width: px(2),
                    height: px(TIMELINE_HEIGHT),
                    border_radius: BorderRadius::all(px(1)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.25, 1.0, 0.45, 1.0)),
                ZIndex(4),
            ));
            tl.spawn((
                TimelineNode(TimelineKind::TrimOut),
                Visibility::Hidden,
                Node {
                    position_type: PositionType::Absolute,
                    left: px(0),
                    top: px(0),
                    width: px(2),
                    height: px(TIMELINE_HEIGHT),
                    border_radius: BorderRadius::all(px(1)),
                    ..default()
                },
                BackgroundColor(Color::srgba(1.0, 0.3, 0.3, 1.0)),
                ZIndex(4),
            ));
        });

    commands.spawn((
        ChildOf(column),
        TimelineTime,
        Visibility::Hidden,
        Node {
            padding: UiRect::axes(px(7), px(2)),
            border_radius: BorderRadius::all(px(3)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.35)),
        Text::new("0.0s / 0.0s"),
        TextFont {
            font: replay_font.clone().into(),
            font_size: FontSize::Px(13.0),
            ..default()
        },
        TextColor(Color::srgb(0.80, 0.85, 0.90)),
    ));

    // Context prompts: the few buttons that do something right now.
    commands
        .spawn((
            ChildOf(column),
            PromptBar,
            Visibility::Hidden,
            Node {
                display: Display::None,
                width: px(TIMELINE_WIDTH),
                flex_direction: FlexDirection::Row,
                flex_wrap: FlexWrap::Wrap,
                align_items: AlignItems::Center,
                column_gap: px(14),
                row_gap: px(6),
                padding: UiRect::axes(px(10), px(7)),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.03, 0.04, 0.80)),
            glass_background(),
            hairline(),
        ))
        .with_children(|bar| {
            for i in 0..PROMPT_SLOTS {
                bar.spawn((
                    PromptEntry(i),
                    Node {
                        display: Display::None,
                        flex_direction: FlexDirection::Row,
                        align_items: AlignItems::Center,
                        column_gap: px(5),
                        ..default()
                    },
                ))
                .with_children(|entry| {
                    entry.spawn((
                        PromptPart::Prefix(i),
                        Node { display: Display::None, ..default() },
                        replay_text(&replay_font, "", 12.0, Color::srgb(0.70, 0.75, 0.80)),
                    ));
                    entry
                        .spawn((
                            PromptChip(i),
                            Node {
                                min_width: px(22),
                                height: px(22),
                                padding: UiRect::axes(px(6), px(0)),
                                align_items: AlignItems::Center,
                                justify_content: JustifyContent::Center,
                                border: UiRect::all(px(1)),
                                border_radius: BorderRadius::all(px(11)),
                                ..default()
                            },
                            BackgroundColor(Color::srgb(0.22, 0.25, 0.30)),
                            BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.18)),
                        ))
                        .with_children(|chip| {
                            chip.spawn((
                                PromptPart::Button(i),
                                Node::default(),
                                replay_text(&replay_font, "", 12.0, Color::WHITE),
                            ));
                        });
                    entry.spawn((
                        PromptPart::Action(i),
                        Node::default(),
                        replay_text(&replay_font, "", 13.0, Color::srgb(0.90, 0.92, 0.95)),
                    ));
                });
            }
        });

    // Camera wheel (hold X): every camera around a ring, centred on screen.
    commands
        .spawn((
            CamWheel,
            UiLayer::Overlay,
            Visibility::Hidden,
            Node {
                display: Display::None,
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                ..default()
            },
            FocusPolicy::Pass,
            GlobalZIndex(19_800),
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    width: px(WHEEL_SIZE),
                    height: px(WHEEL_SIZE),
                    border: UiRect::all(px(1)),
                    border_radius: BorderRadius::all(Val::Percent(50.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.02, 0.03, 0.05, 0.62)),
                hairline(),
                panel_shadow(),
            ))
            .with_children(|ring| {
                let n = frame::REPLAY_CAM_COUNT;
                for cam in 0..n {
                    let a = cam as f32 / n as f32 * std::f32::consts::TAU;
                    let cx = WHEEL_SIZE * 0.5 + WHEEL_RADIUS * a.sin();
                    let cy = WHEEL_SIZE * 0.5 - WHEEL_RADIUS * a.cos();
                    ring.spawn((
                        CamWheelEntry(cam),
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(cx - 50.0),
                            top: px(cy - 26.0),
                            width: px(100),
                            height: px(52),
                            flex_direction: FlexDirection::Column,
                            align_items: AlignItems::Center,
                            justify_content: JustifyContent::Center,
                            row_gap: px(2),
                            border: UiRect::all(px(1)),
                            border_radius: BorderRadius::all(px(8)),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.04)),
                        BorderColor::all(Color::NONE),
                    ))
                    .with_children(|e| {
                        e.spawn((
                            Node { width: px(26), height: px(22), ..default() },
                            ImageNode { image: icons.cams[cam as usize].clone(), color: TINT_IDLE, ..default() },
                        ));
                        e.spawn(replay_text(&replay_font, frame::replay_cam_label(cam), 12.0, Color::srgb(0.85, 0.88, 0.92)));
                    });
                }
                ring.spawn((
                    CamWheelCenter,
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(WHEEL_SIZE * 0.5 - 90.0),
                        top: px(WHEEL_SIZE * 0.5 - 30.0),
                        width: px(180),
                        height: px(60),
                        ..default()
                    },
                    Text::new(""),
                    TextFont { font: replay_font.clone().into(), font_size: FontSize::Px(16.0), ..default() },
                    TextColor(Color::srgb(0.95, 0.96, 0.98)),
                    TextLayout::justify(Justify::Center),
                ));
            });
        });

    // Replay editor menu: the dock's right-hand panel, display:none until
    // Start / Tab opens it, when it slides the timeline column left.
    commands
        .spawn((
            ChildOf(dock),
            ReplayMenu,
            Visibility::Hidden,
            Node {
                display: Display::None,
                width: px(MENU_WIDTH),
                flex_direction: FlexDirection::Column,
                row_gap: px(2),
                padding: UiRect::all(px(10)),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(8)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.03, 0.04, 0.82)),
            glass_background(),
            hairline(),
            panel_shadow(),
        ))
        .with_children(|p| {
            for (i, item) in frame::REPLAY_MENU.iter().enumerate() {
                if let Some(section) = item.section() {
                    p.spawn((
                        replay_text(&replay_font, section, 11.0, Color::srgba(1.0, 0.78, 0.25, 0.75)),
                        Node {
                            margin: UiRect::new(px(8), px(0), px(if i == 0 { 0.0 } else { 6.0 }), px(2)),
                            ..default()
                        },
                    ));
                }
                let kind = item.kind();
                let label_color = if kind == frame::ReplayMenuKind::Hold {
                    Color::srgb(1.0, 0.62, 0.55)
                } else {
                    Color::srgb(0.88, 0.91, 0.94)
                };
                p.spawn((
                    Button,
                    MenuRow(i),
                    Node {
                        flex_direction: FlexDirection::Row,
                        align_items: AlignItems::Center,
                        justify_content: JustifyContent::SpaceBetween,
                        height: px(22),
                        padding: UiRect::axes(px(8), px(0)),
                        border_radius: BorderRadius::all(px(4)),
                        ..default()
                    },
                    BackgroundColor(Color::NONE),
                ))
                .with_children(|row| {
                    row.spawn(replay_text(&replay_font, item.label(), 13.0, label_color));
                    row.spawn(Node {
                        flex_direction: FlexDirection::Row,
                        align_items: AlignItems::Center,
                        column_gap: px(4),
                        ..default()
                    })
                    .with_children(|right| {
                        let adjustable = kind == frame::ReplayMenuKind::Adjust;
                        if adjustable {
                            spawn_menu_arrow(right, &replay_font, i, -1);
                        }
                        right.spawn((
                            MenuValue(i),
                            replay_text(&replay_font, "", 13.0, Color::srgb(0.95, 0.85, 0.40)),
                        ));
                        if adjustable {
                            spawn_menu_arrow(right, &replay_font, i, 1);
                        }
                    });
                });
            }
            p.spawn((
                MenuDescription,
                Node {
                    margin: UiRect::new(px(8), px(8), px(8), px(0)),
                    padding: UiRect::top(px(6)),
                    border: UiRect::top(px(1)),
                    ..default()
                },
                BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.10)),
                replay_text(&replay_font, "", 12.0, Color::srgb(0.70, 0.75, 0.80)),
            ));
        });

    // Load-replay list: takes the menu's place in the dock while open.
    commands
        .spawn((
            ChildOf(dock),
            LoadPanel,
            Visibility::Hidden,
            Node {
                display: Display::None,
                width: px(MENU_WIDTH),
                flex_direction: FlexDirection::Column,
                row_gap: px(2),
                padding: UiRect::all(px(10)),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(8)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.03, 0.04, 0.82)),
            glass_background(),
            hairline(),
            panel_shadow(),
        ))
        .with_children(|p| {
            p.spawn((
                replay_text(&replay_font, "LOAD REPLAY", 11.0, Color::srgba(1.0, 0.78, 0.25, 0.75)),
                Node { margin: UiRect::new(px(8), px(0), px(0), px(4)), ..default() },
            ));
            for i in 0..LOAD_ROWS {
                p.spawn((
                    Button,
                    LoadRow(i),
                    Node {
                        flex_direction: FlexDirection::Column,
                        padding: UiRect::axes(px(8), px(3)),
                        border_radius: BorderRadius::all(px(4)),
                        ..default()
                    },
                    BackgroundColor(Color::NONE),
                ))
                .with_children(|row| {
                    row.spawn((LoadName(i), replay_text(&replay_font, "", 13.0, Color::srgb(0.92, 0.94, 0.97))));
                    row.spawn((LoadInfo(i), replay_text(&replay_font, "", 11.0, Color::srgb(0.62, 0.67, 0.73))));
                });
            }
            p.spawn((
                LoadCount,
                Node { margin: UiRect::new(px(8), px(0), px(6), px(0)), ..default() },
                replay_text(&replay_font, "", 11.0, Color::srgb(0.62, 0.67, 0.73)),
            ));
        });
}

// The replay HUD systems below run every frame, in and out of replays. Bevy
// counts any write through `Mut` as a change, and a changed `Node`,
// `Visibility` or `Text` re-runs UI layout, visibility propagation or text
// shaping for it, so they only write values that actually differ.

fn show(vis: &mut Mut<Visibility>, visible: bool) {
    vis.set_if_neq(if visible { Visibility::Visible } else { Visibility::Hidden });
}

fn set_display(node: &mut Mut<Node>, visible: bool) {
    let display = if visible { Display::Flex } else { Display::None };
    if node.display != display {
        node.display = display;
    }
}

fn set_left(node: &mut Mut<Node>, x: f32) {
    if node.left != px(x) {
        node.left = px(x);
    }
}

fn set_width(node: &mut Mut<Node>, w: f32) {
    if node.width != px(w) {
        node.width = px(w);
    }
}

fn set_text(text: &mut Mut<Text>, want: &str) {
    if text.0 != want {
        text.0 = want.to_string();
    }
}

/// First row of the load list on screen, keeping the focused row in view.
fn load_window_start(focus: usize, len: usize) -> usize {
    if len <= LOAD_ROWS {
        0
    } else {
        focus.saturating_sub(LOAD_ROWS / 2).min(len - LOAD_ROWS)
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn update_load_panel(
    mode: Res<frame::SkateMode>,
    mut panel: Query<(&mut Node, &mut Visibility), (With<LoadPanel>, Without<LoadRow>)>,
    mut rows: Query<(&LoadRow, &Interaction, &mut Node, &mut BackgroundColor), Without<LoadPanel>>,
    mut names: Query<(&LoadName, &mut Text), (Without<LoadInfo>, Without<LoadCount>)>,
    mut infos: Query<(&LoadInfo, &mut Text), (Without<LoadName>, Without<LoadCount>)>,
    mut count: Query<&mut Text, (With<LoadCount>, Without<LoadName>, Without<LoadInfo>)>,
) {
    let visible = mode.replaying && mode.menu_open && mode.load_open && !mode.hide_hud && !mode.rendering;
    for (mut node, mut vis) in &mut panel {
        set_display(&mut node, visible);
        show(&mut vis, visible);
    }
    if !visible {
        return;
    }
    let len = mode.load_entries.len();
    let start = load_window_start(mode.load_focus, len);
    for (LoadRow(i), interaction, mut node, mut bg) in &mut rows {
        let at = start + i;
        set_display(&mut node, at < len);
        bg.set_if_neq(BackgroundColor(if at == mode.load_focus {
            Color::srgba(1.0, 0.85, 0.3, 0.20)
        } else if *interaction == Interaction::Hovered {
            Color::srgba(1.0, 1.0, 1.0, 0.06)
        } else {
            Color::NONE
        }));
    }
    for (LoadName(i), mut text) in &mut names {
        let want = mode.load_entries.get(start + i).map(|e| e.0.as_str()).unwrap_or("");
        set_text(&mut text, want);
    }
    for (LoadInfo(i), mut text) in &mut infos {
        let want = mode.load_entries.get(start + i).map(|e| e.1.as_str()).unwrap_or("");
        set_text(&mut text, want);
    }
    let summary = match len {
        1 => "1 saved replay".to_string(),
        n => format!("{} of {n} saved replays", (mode.load_focus + 1).min(n)),
    };
    for mut text in &mut count {
        set_text(&mut text, &summary);
    }
}

/// Clicks on the load list, handed to the skate adapter.
pub(crate) fn handle_load_mouse(
    mut mode: ResMut<frame::SkateMode>,
    rows: Query<(&Interaction, &LoadRow), Changed<Interaction>>,
) {
    if !mode.load_open {
        return;
    }
    let start = load_window_start(mode.load_focus, mode.load_entries.len());
    for (interaction, LoadRow(i)) in &rows {
        if *interaction == Interaction::Pressed {
            mode.load_click = Some(start + i);
        }
    }
}

/// Marker glyphs, kept so the timeline can add marker nodes on demand.
#[derive(Resource, Clone)]
pub(crate) struct TimelineIcons {
    keyframe: Handle<Image>,
    cut: Handle<Image>,
    ramp: Handle<Image>,
}

fn segment_bundle(i: usize) -> impl Bundle {
    (
        TimelineNode(TimelineKind::Segment(i)),
        Visibility::Hidden,
        Node {
            position_type: PositionType::Absolute,
            left: px(0),
            top: px(TIMELINE_HEIGHT - 5.0),
            width: px(0),
            height: px(5),
            border_radius: BorderRadius::all(px(2)),
            ..default()
        },
        BackgroundColor(Color::srgb(0.3, 0.85, 0.4)),
    )
}

fn ramp_bundle(i: usize, icon: &Handle<Image>) -> impl Bundle {
    (
        TimelineNode(TimelineKind::Ramp(i)),
        Visibility::Hidden,
        Node {
            position_type: PositionType::Absolute,
            left: px(0),
            top: px((TIMELINE_HEIGHT - 8.0) * 0.5),
            width: px(8),
            height: px(8),
            ..default()
        },
        ImageNode { image: icon.clone(), color: TINT_RAMP, ..default() },
        ZIndex(1),
    )
}

fn keyframe_bundle(i: usize, icon: &Handle<Image>) -> impl Bundle {
    (
        TimelineNode(TimelineKind::Keyframe(i)),
        Visibility::Hidden,
        Node {
            position_type: PositionType::Absolute,
            left: px(0),
            top: px((TIMELINE_HEIGHT - 12.0) * 0.5),
            width: px(12),
            height: px(12),
            ..default()
        },
        ImageNode { image: icon.clone(), color: TINT_KEYFRAME, ..default() },
        ZIndex(1),
    )
}

fn cut_bundle(i: usize, icon: &Handle<Image>) -> impl Bundle {
    (
        TimelineNode(TimelineKind::Cut(i)),
        Visibility::Hidden,
        Node {
            position_type: PositionType::Absolute,
            left: px(0),
            top: px(2),
            width: px(12),
            height: px(12),
            ..default()
        },
        ImageNode { image: icon.clone(), color: TINT_CUT, ..default() },
        ZIndex(2),
    )
}

/// Marker x position (left edge) on the timeline for a glyph `w` wide.
fn marker_left(t: f32, duration: f32, w: f32) -> f32 {
    (t / duration).clamp(0.0, 1.0) * (TIMELINE_WIDTH - w)
}

/// Mouse on the replay timeline, handed to the skate adapter as events:
/// press on a trim line or a marker to drag it, anywhere else to scrub.
pub(crate) fn handle_timeline_mouse(
    mut mode: ResMut<frame::SkateMode>,
    timeline: Query<(&Interaction, &bevy::ui::RelativeCursorPosition), With<SkateTimeline>>,
    mut held: Local<Option<f32>>,
) {
    let Ok((interaction, cursor)) = timeline.single() else { return };
    let active = mode.replaying && !mode.hide_hud && !mode.rendering && !mode.menu_open;
    let pressed = active && *interaction == Interaction::Pressed;
    let duration = mode.replay_duration.max(0.001);
    // RelativeCursorPosition runs -0.5..0.5 across the node.
    let x = cursor.normalized.map(|n| (n.x + 0.5).clamp(0.0, 1.0));
    match (pressed, *held, x) {
        (true, None, Some(x)) => {
            let px_at = x * TIMELINE_WIDTH;
            let t = x * duration;
            let near = |left: f32, w: f32| (left + w * 0.5 - px_at).abs();
            let mut best: Option<(f32, frame::TimelineHit)> = None;
            let mut consider = |d: f32, hit: frame::TimelineHit| {
                if d <= 7.0 && best.is_none_or(|(b, _)| d < b) {
                    best = Some((d, hit));
                }
            };
            if mode.trim_start > 0.0 {
                consider(near(marker_left(mode.trim_start, duration, 2.0), 2.0), frame::TimelineHit::TrimIn);
            }
            if mode.trim_end > 0.0 {
                consider(near(marker_left(mode.trim_end, duration, 2.0), 2.0), frame::TimelineHit::TrimOut);
            }
            for (i, (ct, _)) in mode.cut_times.iter().enumerate() {
                consider(near(marker_left(*ct, duration, 12.0), 12.0), frame::TimelineHit::Cut(i));
            }
            for (i, kt) in mode.keyframe_times.iter().enumerate() {
                consider(near(marker_left(*kt, duration, 12.0), 12.0), frame::TimelineHit::Keyframe(i));
            }
            for (i, (rt, _)) in mode.speed_ramps.iter().enumerate() {
                consider(near(marker_left(*rt, duration, 8.0), 8.0), frame::TimelineHit::Ramp(i));
            }
            let hit = best.map(|(_, h)| h).unwrap_or(frame::TimelineHit::Track);
            mode.timeline_mouse.push(frame::TimelineMouse::Press(hit, t));
            *held = Some(t);
        }
        (true, Some(last), Some(x)) => {
            let t = x * duration;
            if (t - last).abs() > 1.0e-4 {
                mode.timeline_mouse.push(frame::TimelineMouse::Drag(t));
                *held = Some(t);
            }
        }
        (false, Some(_), _) => {
            mode.timeline_mouse.push(frame::TimelineMouse::Release);
            *held = None;
        }
        _ => {}
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn update_skate_timeline(
    mode: Res<frame::SkateMode>,
    mut commands: Commands,
    icons: Option<Res<TimelineIcons>>,
    mut timeline: Query<(Entity, &mut Visibility), (With<SkateTimeline>, Without<TimelineNode>)>,
    mut nodes: Query<
        (
            &TimelineNode,
            &mut Node,
            &mut Visibility,
            Option<&mut BackgroundColor>,
            Option<&mut ImageNode>,
            Option<&mut BorderColor>,
        ),
        Without<SkateTimeline>,
    >,
) {
    let visible = mode.replaying && !mode.hide_hud && !mode.rendering;
    let mut track = None;
    for (entity, mut vis) in &mut timeline {
        show(&mut vis, visible);
        track = Some(entity);
    }
    // Grow the marker pools when a clip has more markers than nodes, so no
    // marker is ever left undrawn.
    if visible && let (Some(track), Some(icons)) = (track, icons.as_deref()) {
        let (mut cuts, mut keys, mut ramps, mut segs) = (0usize, 0usize, 0usize, 0usize);
        for (node, ..) in &nodes {
            match node.0 {
                TimelineKind::Cut(i) => cuts = cuts.max(i + 1),
                TimelineKind::Keyframe(i) => keys = keys.max(i + 1),
                TimelineKind::Ramp(i) => ramps = ramps.max(i + 1),
                TimelineKind::Segment(i) => segs = segs.max(i + 1),
                _ => {}
            }
        }
        for i in cuts..mode.cut_times.len() {
            commands.spawn((ChildOf(track), cut_bundle(i, &icons.cut)));
        }
        for i in keys..mode.keyframe_times.len() {
            commands.spawn((ChildOf(track), keyframe_bundle(i, &icons.keyframe)));
        }
        for i in ramps..mode.speed_ramps.len() {
            commands.spawn((ChildOf(track), ramp_bundle(i, &icons.ramp)));
        }
        for i in segs..=mode.speed_ramps.len() {
            commands.spawn((ChildOf(track), segment_bundle(i)));
        }
    }
    let duration = mode.replay_duration.max(0.001);
    for (node, mut style, mut vis, bg, _icon, border) in &mut nodes {
        // `Visibility::Visible` is unconditional in Bevy (it overrides a hidden
        // parent), so explicitly hide every node when the timeline is off —
        // otherwise the playhead/markers leak into the exported frames.
        if !visible {
            show(&mut vis, false);
            continue;
        }
        // Where the node goes this frame, or None to hide it.
        let left = match node.0 {
            TimelineKind::Playhead => Some((mode.replay_clock / duration).clamp(0.0, 1.0) * (TIMELINE_WIDTH - 2.0)),
            TimelineKind::Keyframe(i) => mode.keyframe_times.get(i).map(|&t| marker_left(t, duration, 12.0)),
            TimelineKind::Ramp(i) => mode.speed_ramps.get(i).map(|&(t, _)| marker_left(t, duration, 8.0)),
            TimelineKind::Cut(i) => mode.cut_times.get(i).map(|&(t, _)| marker_left(t, duration, 12.0)),
            TimelineKind::Segment(i) => segment_bounds(&mode.speed_ramps, duration, i).map(|(start, end, speed)| {
                let x0 = (start / duration).clamp(0.0, 1.0) * TIMELINE_WIDTH;
                let x1 = (end / duration).clamp(0.0, 1.0) * TIMELINE_WIDTH;
                set_width(&mut style, (x1 - x0).max(0.0));
                if let Some(mut bg) = bg {
                    bg.set_if_neq(BackgroundColor(speed_color(speed)));
                }
                x0
            }),
            TimelineKind::TrimIn => (mode.trim_start > 0.0).then(|| marker_left(mode.trim_start, duration, 2.0)),
            TimelineKind::TrimOut => (mode.trim_end > 0.0).then(|| marker_left(mode.trim_end, duration, 2.0)),
            TimelineKind::ShadeLeft => (mode.trim_start > 0.0).then(|| {
                set_width(&mut style, (mode.trim_start / duration).clamp(0.0, 1.0) * TIMELINE_WIDTH);
                0.0
            }),
            TimelineKind::ShadeRight => (mode.trim_end > 0.0).then(|| {
                let x = (mode.trim_end / duration).clamp(0.0, 1.0) * TIMELINE_WIDTH;
                set_width(&mut style, TIMELINE_WIDTH - x);
                x
            }),
            TimelineKind::Target => {
                // Yellow round a marker being moved, red round the one a
                // hold-B would delete.
                let (at, color) = match (&mode.grabbed, &mode.delete_target) {
                    (Some((t, _)), _) => (Some(*t), Color::srgba(1.0, 0.85, 0.3, 0.95)),
                    (None, Some((t, _))) => (Some(*t), Color::srgba(1.0, 0.45, 0.40, 0.95)),
                    _ => (None, Color::NONE),
                };
                at.filter(|_| !mode.menu_open).map(|t| {
                    if let Some(mut border) = border {
                        border.set_if_neq(BorderColor::all(color));
                    }
                    marker_left(t, duration, 12.0) + 6.0 - 9.0
                })
            }
        };
        match left {
            Some(x) => {
                show(&mut vis, true);
                set_left(&mut style, x);
            }
            None => show(&mut vis, false),
        }
    }
}

/// Speed segments under the timeline: normal speed until the first ramp,
/// then each ramp's speed until the next (how `clip_speed` plays it).
fn segment_bounds(ramps: &[(f32, f32)], duration: f32, i: usize) -> Option<(f32, f32, f32)> {
    let n = ramps.len();
    if n == 0 || i > n {
        return None;
    }
    if i == 0 {
        return Some((0.0, ramps[0].0, 1.0));
    }
    let (start, speed) = ramps[i - 1];
    let end = ramps.get(i).map(|r| r.0).unwrap_or(duration);
    Some((start, end, speed))
}

pub(crate) fn update_skate_time(
    mode: Res<frame::SkateMode>,
    mut label: Query<(&mut Text, &mut Visibility), With<TimelineTime>>,
) {
    let visible = mode.replaying && !mode.hide_hud && !mode.rendering;
    for (mut text, mut vis) in &mut label {
        show(&mut vis, visible);
        if visible {
            // Shown to a tenth of a second: re-shaping the text every frame
            // for an unchanged string is wasted work.
            let want = format!("{:.1}s / {:.1}s", mode.replay_clock, mode.replay_duration);
            set_text(&mut text, &want);
        }
    }
}

pub(crate) fn update_camera_strip(
    mode: Res<frame::SkateMode>,
    mut strip: Query<&mut Visibility, With<CameraStrip>>,
    mut entries: Query<(&CameraStripEntry, &mut BackgroundColor, &mut ImageNode)>,
    mut label: Query<&mut Text, With<CameraStripLabel>>,
) {
    let visible = mode.replaying && !mode.hide_hud && !mode.rendering;
    for mut vis in &mut strip {
        show(&mut vis, visible);
    }
    if !visible {
        return;
    }
    let name = frame::replay_cam_label(mode.eff_cam);
    let text = if frame::replay_cam_steerable(mode.eff_cam) && mode.cam_control {
        format!("{name}  ·  camera control")
    } else {
        name.to_string()
    };
    for mut t in &mut label {
        set_text(&mut t, &text);
    }
    for (CameraStripEntry(i), mut bg, mut icon) in &mut entries {
        let (tint, pill) = if Some(*i) == mode.active_cut_cam {
            // Cyan: the camera a cut is currently showing.
            (TINT_CUT, Color::srgba(0.35, 0.80, 1.0, 0.16))
        } else if *i == mode.replay_cam {
            (TINT_ACTIVE, Color::srgba(1.0, 0.85, 0.3, 0.16))
        } else {
            (TINT_IDLE, Color::srgba(1.0, 1.0, 1.0, 0.04))
        };
        if icon.color != tint {
            icon.color = tint;
        }
        bg.set_if_neq(BackgroundColor(pill));
    }
}

pub(crate) fn update_replay_menu(
    mode: Res<frame::SkateMode>,
    mut panel: Query<(&mut Node, &mut Visibility), With<ReplayMenu>>,
    mut rows: Query<(&MenuRow, &Interaction, &mut BackgroundColor)>,
    mut values: Query<(&MenuValue, &mut Text), Without<MenuDescription>>,
    mut description: Query<&mut Text, (With<MenuDescription>, Without<MenuValue>)>,
) {
    let visible =
        mode.replaying && mode.menu_open && !mode.load_open && !mode.hide_hud && !mode.rendering;
    for (mut node, mut vis) in &mut panel {
        set_display(&mut node, visible);
        show(&mut vis, visible);
    }
    if !visible {
        return;
    }
    for (MenuRow(i), interaction, mut bg) in &mut rows {
        bg.set_if_neq(BackgroundColor(if *i == mode.menu_focus {
            Color::srgba(1.0, 0.85, 0.3, 0.20)
        } else if *interaction == Interaction::Hovered {
            Color::srgba(1.0, 1.0, 1.0, 0.06)
        } else {
            Color::NONE
        }));
    }
    for (MenuValue(i), mut text) in &mut values {
        let value = mode.menu_values.get(*i).map(String::as_str).unwrap_or("");
        set_text(&mut text, value);
    }
    let focus = mode.menu_focus.min(frame::REPLAY_MENU.len() - 1);
    let about = frame::REPLAY_MENU[focus].description();
    for mut text in &mut description {
        set_text(&mut text, about);
    }
}

/// Colours of a prompt chip: (fill, text). Face buttons use the pad's own
/// colours; everything else is a neutral cap. Keyboard keys are light keycaps.
fn chip_colors(button: &str, pad: bool) -> (Color, Color) {
    if !pad {
        return (Color::srgb(0.86, 0.88, 0.91), Color::srgb(0.08, 0.09, 0.11));
    }
    match button {
        "A" => (Color::srgb(0.36, 0.70, 0.22), Color::WHITE),
        "B" => (Color::srgb(0.85, 0.25, 0.22), Color::WHITE),
        "X" => (Color::srgb(0.20, 0.47, 0.92), Color::WHITE),
        "Y" => (Color::srgb(0.95, 0.76, 0.16), Color::srgb(0.10, 0.08, 0.02)),
        _ => (Color::srgb(0.22, 0.25, 0.30), Color::srgb(0.93, 0.95, 0.97)),
    }
}

/// Splits "Hold B" / "Release X" into the prefix word and the button.
fn split_prompt_key(key: &str) -> (&str, &str) {
    for prefix in ["Hold ", "Release "] {
        if let Some(rest) = key.strip_prefix(prefix) {
            return (prefix.trim_end(), rest);
        }
    }
    ("", key)
}

#[allow(clippy::type_complexity)]
pub(crate) fn update_prompt_bar(
    mode: Res<frame::SkateMode>,
    mut bar: Query<(&mut Node, &mut Visibility), (With<PromptBar>, Without<PromptEntry>, Without<PromptChip>, Without<PromptPart>)>,
    mut entries: Query<(&PromptEntry, &mut Node), (Without<PromptBar>, Without<PromptChip>, Without<PromptPart>)>,
    mut chips: Query<(&PromptChip, &mut Node, &mut BackgroundColor), (Without<PromptBar>, Without<PromptEntry>, Without<PromptPart>)>,
    mut parts: Query<(&PromptPart, &mut Node, &mut Text, &mut TextColor), (Without<PromptBar>, Without<PromptEntry>, Without<PromptChip>)>,
) {
    let visible = mode.replaying && !mode.hide_hud && !mode.rendering && !mode.prompts.is_empty();
    for (mut node, mut vis) in &mut bar {
        set_display(&mut node, visible);
        show(&mut vis, visible);
    }
    if !visible {
        return;
    }
    let prompts = &mode.prompts;
    for (PromptEntry(i), mut node) in &mut entries {
        set_display(&mut node, *i < prompts.len());
    }
    for (PromptChip(i), mut node, mut bg) in &mut chips {
        let Some((key, _)) = prompts.get(*i) else { continue };
        let (_, button) = split_prompt_key(key);
        let (fill, _) = chip_colors(button, mode.pad_prompts);
        bg.set_if_neq(BackgroundColor(fill));
        // Single face buttons are round; longer names are pills / keycaps.
        let radius = BorderRadius::all(px(if mode.pad_prompts { 11.0 } else { 4.0 }));
        if node.border_radius != radius {
            node.border_radius = radius;
        }
    }
    for (part, mut node, mut text, mut color) in &mut parts {
        let (i, want, show) = match *part {
            PromptPart::Prefix(i) => {
                let key = prompts.get(i).map(|(k, _)| k.as_str()).unwrap_or("");
                let (prefix, _) = split_prompt_key(key);
                (i, prefix.to_string(), !prefix.is_empty())
            }
            PromptPart::Button(i) => {
                let key = prompts.get(i).map(|(k, _)| k.as_str()).unwrap_or("");
                let (_, button) = split_prompt_key(key);
                color.set_if_neq(TextColor(chip_colors(button, mode.pad_prompts).1));
                (i, button.to_string(), true)
            }
            PromptPart::Action(i) => {
                let action = prompts.get(i).map(|(_, a)| a.as_str()).unwrap_or("");
                (i, action.to_string(), true)
            }
        };
        let _ = i;
        set_display(&mut node, show);
        set_text(&mut text, &want);
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn update_cam_wheel(
    mode: Res<frame::SkateMode>,
    mut wheel: Query<(&mut Node, &mut Visibility), (With<CamWheel>, Without<CamWheelEntry>)>,
    mut entries: Query<(&CamWheelEntry, &mut BackgroundColor, &mut BorderColor, &Children)>,
    mut icons: Query<&mut ImageNode>,
    mut center: Query<&mut Text, With<CamWheelCenter>>,
) {
    let pick = mode.cam_wheel;
    let visible = mode.replaying && !mode.rendering && pick.is_some();
    for (mut node, mut vis) in &mut wheel {
        set_display(&mut node, visible);
        show(&mut vis, visible);
    }
    let Some(pick) = pick else { return };
    for (CamWheelEntry(cam), mut bg, mut border, children) in &mut entries {
        let (fill, edge, tint) = if *cam == pick {
            (Color::srgba(1.0, 0.85, 0.3, 0.22), Color::srgba(1.0, 0.85, 0.3, 0.9), TINT_ACTIVE)
        } else if *cam == mode.eff_cam {
            (Color::srgba(0.35, 0.80, 1.0, 0.12), Color::srgba(0.35, 0.80, 1.0, 0.45), TINT_CUT)
        } else {
            (Color::srgba(1.0, 1.0, 1.0, 0.04), Color::NONE, TINT_IDLE)
        };
        bg.set_if_neq(BackgroundColor(fill));
        border.set_if_neq(BorderColor::all(edge));
        for child in children.iter() {
            if let Ok(mut icon) = icons.get_mut(child)
                && icon.color != tint
            {
                icon.color = tint;
            }
        }
    }
    let text = format!("{}\nRelease X to cut", frame::replay_cam_label(pick));
    for mut t in &mut center {
        set_text(&mut t, &text);
    }
}

/// Mouse input for the replay menu, handed to the skate adapter as intents.
pub(crate) fn handle_replay_menu_mouse(
    mut mode: ResMut<frame::SkateMode>,
    rows: Query<(&Interaction, &MenuRow), Changed<Interaction>>,
    arrows: Query<(&Interaction, &MenuArrow), Changed<Interaction>>,
    held: Query<(&Interaction, &MenuRow)>,
) {
    if !mode.menu_open {
        mode.menu_mouse_hold = None;
        return;
    }
    for (interaction, MenuRow(row)) in &rows {
        if *interaction == Interaction::Pressed {
            mode.menu_focus = *row;
            // Hold rows fire from the held button below, never on a click.
            if frame::REPLAY_MENU[*row].kind() != frame::ReplayMenuKind::Hold {
                mode.menu_click = Some((*row, 0));
            }
        }
    }
    for (interaction, MenuArrow(row, dir)) in &arrows {
        if *interaction == Interaction::Pressed {
            mode.menu_click = Some((*row, *dir));
        }
    }
    mode.menu_mouse_hold = held
        .iter()
        .find(|(interaction, MenuRow(row))| {
            **interaction == Interaction::Pressed
                && frame::REPLAY_MENU[*row].kind() == frame::ReplayMenuKind::Hold
        })
        .map(|(_, MenuRow(row))| *row);
}

pub(crate) fn update_portrait_guide(
    mode: Res<frame::SkateMode>,
    mut guide: Query<&mut Visibility, With<PortraitGuide>>,
) {
    let visible = mode.replaying && mode.portrait && !mode.hide_hud && !mode.rendering;
    for mut vis in &mut guide {
        let want = if visible { Visibility::Visible } else { Visibility::Hidden };
        if *vis != want {
            *vis = want;
        }
    }
}

pub(crate) fn update_cinematic_bars(
    mode: Res<frame::SkateMode>,
    mut bars: Query<&mut Visibility, With<CinematicBars>>,
) {
    // Shown during renders too, as a preview: the bars sit outside the
    // ffmpeg crop, so they are cut away before encoding and can never appear
    // in the video file itself.
    let visible =
        mode.replaying && mode.cinematic && (!mode.hide_hud || mode.rendering);
    for mut vis in &mut bars {
        let want = if visible { Visibility::Visible } else { Visibility::Hidden };
        if *vis != want {
            *vis = want;
        }
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn update_replay_toast(
    mode: Res<frame::SkateMode>,
    mut toast: Query<
        (&mut Node, &mut Visibility),
        (With<ReplayToast>, Without<ReplayHoldTrack>, Without<ReplayHoldBar>),
    >,
    mut text: Query<&mut Text, With<ReplayToastText>>,
    mut track: Query<&mut Node, (With<ReplayHoldTrack>, Without<ReplayToast>, Without<ReplayHoldBar>)>,
    mut bar: Query<&mut Node, (With<ReplayHoldBar>, Without<ReplayToast>, Without<ReplayHoldTrack>)>,
) {
    // A hold in progress takes the strip over from a toast.
    let (message, progress) = match (&mode.hold, &mode.toast) {
        (Some((label, p)), _) => (Some(label.as_str()), Some(*p)),
        (None, Some((message, _))) => (Some(message.as_str()), None),
        _ => (None, None),
    };
    let visible = mode.replaying && !mode.hide_hud && !mode.rendering && message.is_some();
    for (mut node, mut vis) in &mut toast {
        set_display(&mut node, visible);
        show(&mut vis, visible);
    }
    if let Some(message) = message {
        for mut t in &mut text {
            set_text(&mut t, message);
        }
    }
    for mut node in &mut track {
        set_display(&mut node, progress.is_some());
    }
    for mut node in &mut bar {
        set_width(&mut node, progress.unwrap_or(0.0) * HOLD_BAR_WIDTH);
    }
}

pub(crate) fn register_debug_move_commands(registry: &mut ConsoleRegistry) {
    registry.register(crate::CommandSpec::new("skate").usage("skate [on|off|record|replay|instant|cam [<n|name>]|speed <n>|fov <n>|roll <n>|keyframe [<time>|clear]|cut [<n|name>|clear]|ease <smooth|linear>|save <name>|load <name>|render <fps> [shutter] [res] [crf] [name]|skin <name|next|prev|list>|status] - local Skate gameplay (J toggles)"));
    if registry.resolve("showpos").is_none() {
        registry.register(
            crate::CommandSpec::new("showpos")
                .alias("debug_pos")
                .usage("showpos [on|off] — origin overlay (bare also prints once)"),
        );
    }
    if registry.resolve("move").is_none() {
        registry.register(
            crate::CommandSpec::new("move")
                .alias("tp")
                .usage("move <x> <y> <z> [yaw] [pitch] — write player origin (needs cheats)"),
        );
    }
    if registry.resolve("kill").is_none() {
        registry
            .register(crate::CommandSpec::new("kill").usage(
                "kill — ForceDeath the local player (needs cheats; invented, not COMMANDS)",
            ));
    }
    if registry.resolve("force_spawn").is_none() {
        registry.register(crate::CommandSpec::new("force_spawn").usage(
            "force_spawn [random <seed> | at <x> <y> <z> [yaw]] — respawn from any state through spawn resolution (needs cheats)",
        ));
    }
    if registry.resolve("damage").is_none() {
        registry.register(
            crate::CommandSpec::new("damage").usage(
                "damage [amount] — subtract Alive health (default 40; needs cheats; invented)",
            ),
        );
    }
    if registry.resolve("look").is_none() {
        registry.register(
            crate::CommandSpec::new("look")
                .usage("look <yaw> <pitch> — write LookState + authority angles (needs cheats)"),
        );
    }
    if registry.resolve("name").is_none() {
        registry.register(
            crate::CommandSpec::new("name")
                .usage("name [string] — write clientState.name[16] for the local client"),
        );
    }
    if registry.resolve("nudge").is_none() {
        registry.register(crate::CommandSpec::new("nudge").usage(
            "nudge <dx> <dy> <dz> — authority-only origin shift, no teleport bit (needs cheats)",
        ));
    }
    if registry.resolve("force_match_start").is_none() {
        registry.register(crate::CommandSpec::new("force_match_start").usage(
            "force_match_start — skip waitForPlayers and matchStartTimer (needs cheats; invented)",
        ));
    }
}

pub(crate) fn route_debug_move_commands(
    mut skate: ResMut<frame::SkateMode>,
    mut events: MessageReader<ConsoleCommand>,
    mut console: ResMut<ConsoleState>,
    settings: Res<ConsoleSettings>,
    mut line: ResMut<ConsoleLine>,
    mut debug_pos: ResMut<DebugPosOverlay>,
    presented: Res<PresentedSnapshot>,
    local: Res<LocalPresentClient>,
    mut authority: Option<ResMut<net::AuthorityWorld>>,
    mut inbox: Option<ResMut<ClientActionInbox>>,
    mut seq: ResMut<net::ActionRequestIds>,
    mut look: ResMut<LookState>,
    mut dispatch: ResMut<ConsoleDispatch>,
) {
    let capacity = settings.log_capacity;
    let echo = |msg: String, console: &mut ConsoleState, line: &mut ConsoleLine| {
        diag::info!(Console, "{msg}");
        line.0 = msg.clone();
        console.echo(msg, capacity);
    };

    for cmd in events.read() {
        match cmd.name.as_str() {
            "skate" => {
                match cmd.args.first().map(String::as_str) {
                    Some("status") => {},
                    Some("on") => { skate.toggle_requested = !skate.active && !skate.entering; },
                    Some("off") => { skate.toggle_requested = skate.active || skate.entering; },
                    Some("record") => { skate.record_requested = true; },
                    Some("replay") => { skate.replay_requested = true; },
                    Some("instant") => { skate.instant_requested = true; },
                    Some("cam") => match cmd.args.get(1).map(String::as_str) {
                        None => { skate.cam_requested = true; }
                        Some(arg) => match arg.parse::<u8>().ok().or_else(|| cam_index(arg)) {
                            Some(c) => { skate.cam_set_requested = Some(c); }
                            None => { echo(format!("skate cam: unknown camera `{arg}`"), &mut console, &mut line); continue; }
                        },
                    },
                    Some("keyframe") => match cmd.args.get(1).map(String::as_str) {
                        Some("clear") => { skate.keyframe_clear_requested = true; }
                        other => {
                            skate.keyframe_add_time = other.and_then(|s| s.parse::<f32>().ok());
                            skate.keyframe_add_requested = true;
                        }
                    },
                    Some("cut") => match cmd.args.get(1).map(String::as_str) {
                        Some("clear") => { skate.cut_clear_requested = true; }
                        None => { skate.cut_add_requested = true; }
                        Some(arg) => match arg.parse::<u8>().ok().or_else(|| cam_index(arg)) {
                            Some(c) => { skate.cut_to_requested = Some(c); }
                            None => { echo(format!("skate cut: unknown camera `{arg}`"), &mut console, &mut line); continue; }
                        },
                    },
                    Some("ease") => match cmd.args.get(1).map(String::as_str) {
                        Some("smooth") => { skate.ease_requested = Some(true); }
                        Some("linear") => { skate.ease_requested = Some(false); }
                        _ => { echo("usage: skate ease <smooth|linear>".into(), &mut console, &mut line); continue; }
                    },
                    Some("speed") => match cmd.args.get(1).and_then(|s| s.parse::<f32>().ok()) {
                        Some(n) if n > 0.0 => { skate.replay_speed = n.clamp(0.125, 4.0); }
                        _ => { echo("usage: skate speed <0.125..4>".into(), &mut console, &mut line); continue; }
                    },
                    Some("fov") => match cmd.args.get(1).and_then(|s| s.parse::<f32>().ok()) {
                        Some(n) => { skate.replay_fov = n.clamp(10.0, 120.0); echo(format!("replay fov: {:.0}", skate.replay_fov), &mut console, &mut line); }
                        _ => { echo("usage: skate fov <10..120>".into(), &mut console, &mut line); continue; }
                    },
                    Some("roll") => match cmd.args.get(1).and_then(|s| s.parse::<f32>().ok()) {
                        Some(n) => { skate.replay_roll = n.clamp(-0.6, 0.6); echo(format!("replay roll: {:.2}", skate.replay_roll), &mut console, &mut line); }
                        _ => { echo("usage: skate roll <-0.6..0.6>".into(), &mut console, &mut line); continue; }
                    },
                    Some("save") => match cmd.args.get(1) {
                        Some(name) => { skate.save_requested = Some(name.clone()); }
                        None => { echo("usage: skate save <name>".into(), &mut console, &mut line); continue; }
                    },
                    Some("load") => match cmd.args.get(1) {
                        Some(name) => { skate.load_requested = Some(name.clone()); }
                        None => { echo("usage: skate load <name>".into(), &mut console, &mut line); continue; }
                    },
                    Some("render") => {
                        // Same limits as the editor menu (a typo like 6000 fps
                        // would otherwise queue hours of capture); CRF caps at
                        // the encoders' own 51.
                        let fps = cmd.args.get(1).and_then(|s| s.parse::<f32>().ok()).filter(|f| f.is_finite()).unwrap_or(60.0).clamp(1.0, 240.0);
                        let shutter = cmd.args.get(2).and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).clamp(1, 16);
                        let res = cmd.args.get(3).cloned().unwrap_or_else(String::new);
                        let crf = cmd.args.get(4).and_then(|s| s.parse::<u32>().ok()).unwrap_or(18).min(51);
                        let name = cmd.args.get(5).cloned().unwrap_or_else(|| "clip".to_string());
                        skate.render_fps = fps;
                        skate.render_shutter = shutter;
                        skate.render_crf = crf;
                        skate.render_scale = res;
                        skate.render_name = name;
                        skate.render_requested = true;
                        echo(format!("skate render: {fps}fps shutter={shutter} crf={crf}"), &mut console, &mut line);
                    },
                    Some("skin") => match cmd.args.get(1).map(String::as_str) {
                        None | Some("next") => { assets::bot_model::cycle_skin(1); }
                        Some("prev") => { assets::bot_model::cycle_skin(-1); }
                        Some("list") => {
                            let list = assets::bot_model::skin_names()
                                .iter()
                                .enumerate()
                                .map(|(i, n)| format!("{i}:{n}"))
                                .collect::<Vec<_>>()
                                .join("  ");
                            echo(format!("skins: {list}"), &mut console, &mut line);
                            continue;
                        }
                        Some(name) => {
                            let names = assets::bot_model::skin_names();
                            if let Some(index) = names.iter().position(|n| n == name) {
                                assets::bot_model::select_skin(index);
                            } else {
                                echo(format!("skate skin: unknown skin `{name}`"), &mut console, &mut line);
                                continue;
                            }
                        }
                    },
                    None => { skate.toggle_requested = true; },
                    _ => { echo("usage: skate [on|off|record|replay|instant|cam [<n|name>]|speed <n>|fov <n>|roll <n>|keyframe [<time>|clear]|cut [<n|name>|clear]|ease <smooth|linear>|save <name>|load <name>|render <fps> [shutter] [res] [crf] [name]|skin <name|next|prev|list>|status]".into(), &mut console, &mut line); continue; }
                }
                echo(format!("skate active={} ready={} controller={:?} tick={} recording={} replaying={} cam={}({}) speed={:.2} kf={} cuts={} skin={} {}",skate.active,skate.preloaded,skate.controller,skate.tick,skate.recording,skate.replaying,skate.replay_cam,cam_name(skate.replay_cam),skate.replay_speed,skate.keyframe_times.len(),skate.cut_times.len(),current_skin_name(),skate.status),&mut console,&mut line);
            }

            "showpos" | "debug_pos" => match cmd.args.first().map(String::as_str) {
                None => {
                    debug_pos.0 = true;
                    echo(format_showpos(&presented, local.0), &mut console, &mut line);
                }
                Some("on") | Some("1") => {
                    debug_pos.0 = true;
                    echo("showpos on".into(), &mut console, &mut line);
                }
                Some("off") | Some("0") => {
                    debug_pos.0 = false;
                    echo("showpos off".into(), &mut console, &mut line);
                }
                Some(other) => echo(
                    format!("usage: showpos [on|off] (got `{other}`)"),
                    &mut console,
                    &mut line,
                ),
            },
            "move" | "tp" => match parse_move(&cmd.args, &presented, local.0) {
                Err(msg) => echo(msg, &mut console, &mut line),
                Ok((origin, angles)) => {
                    if !alive(&presented, local.0) {
                        echo(
                            "move: not Alive — spawn a class first".into(),
                            &mut console,
                            &mut line,
                        );
                        continue;
                    }
                    if authority.as_ref().is_some_and(|a| !a.0.cheats_enabled()) {
                        echo("move: cheats are off".into(), &mut console, &mut line);
                        continue;
                    }
                    let Some(inbox) = inbox.as_deref_mut() else {
                        echo(
                            "move: no action inbox (not a listen host)".into(),
                            &mut console,
                            &mut line,
                        );
                        continue;
                    };
                    let request_id = seq.allocate();
                    if let Err(error) = inbox.push(
                        local.0,
                        ClientAction::Move {
                            request_id,
                            origin,
                            angles,
                        },
                    ) {
                        echo(format!("move: {error}"), &mut console, &mut line);
                        continue;
                    }

                    look.angles = look_angles_from_degrees(angles);
                    arm_wait_move(
                        &mut dispatch,
                        cmd.background,
                        local.0,
                        origin,
                        angles,
                        "move",
                    );
                    echo(
                        format!(
                            "move: queued ({:.1} {:.1} {:.1}) yaw={:.0} pitch={:.0} request_id={request_id}{}",
                            origin[0],
                            origin[1],
                            origin[2],
                            angles[1],
                            angles[0],
                            if cmd.background {
                                " (async)"
                            } else {
                                " (sync until presented pose)"
                            }
                        ),
                        &mut console,
                        &mut line,
                    );
                }
            },
            "look" => match parse_look(&cmd.args, &presented, local.0) {
                Err(msg) => echo(msg, &mut console, &mut line),
                Ok((origin, angles)) => {
                    if !alive(&presented, local.0) {
                        echo(
                            "look: not Alive — spawn a class first".into(),
                            &mut console,
                            &mut line,
                        );
                        continue;
                    }
                    if authority.as_ref().is_some_and(|a| !a.0.cheats_enabled()) {
                        echo("look: cheats are off".into(), &mut console, &mut line);
                        continue;
                    }
                    let Some(inbox) = inbox.as_deref_mut() else {
                        echo(
                            "look: no action inbox (not a listen host)".into(),
                            &mut console,
                            &mut line,
                        );
                        continue;
                    };
                    let request_id = seq.allocate();
                    if let Err(error) = inbox.push(
                        local.0,
                        ClientAction::Move {
                            request_id,
                            origin,
                            angles,
                        },
                    ) {
                        echo(format!("look: {error}"), &mut console, &mut line);
                        continue;
                    }
                    look.angles = look_angles_from_degrees(angles);
                    arm_wait_move(
                        &mut dispatch,
                        cmd.background,
                        local.0,
                        origin,
                        angles,
                        "look",
                    );
                    echo(
                        format!(
                            "look: queued yaw={:.0} pitch={:.0} request_id={request_id}{}",
                            angles[1],
                            angles[0],
                            if cmd.background {
                                " (async)"
                            } else {
                                " (sync until presented pose)"
                            }
                        ),
                        &mut console,
                        &mut line,
                    );
                }
            },
            "kill" => {
                if !alive(&presented, local.0) {
                    echo(
                        "kill: not Alive — spawn a class first".into(),
                        &mut console,
                        &mut line,
                    );
                    continue;
                }
                if authority.as_ref().is_some_and(|a| !a.0.cheats_enabled()) {
                    echo("kill: cheats are off".into(), &mut console, &mut line);
                    continue;
                }
                let Some(inbox) = inbox.as_deref_mut() else {
                    echo(
                        "kill: no action inbox (not a listen host)".into(),
                        &mut console,
                        &mut line,
                    );
                    continue;
                };
                let request_id = seq.allocate();
                if let Err(error) = inbox.push(local.0, ClientAction::ForceDeath { request_id }) {
                    echo(format!("kill: {error}"), &mut console, &mut line);
                    continue;
                }
                echo(
                    format!("kill: queued ForceDeath request_id={request_id}"),
                    &mut console,
                    &mut line,
                );
            }
            "force_spawn" => {
                let pick = match parse_force_spawn(&cmd.args) {
                    Ok(pick) => pick,
                    Err(error) => {
                        echo(format!("force_spawn: {error}"), &mut console, &mut line);
                        continue;
                    }
                };
                if authority.as_ref().is_some_and(|a| !a.0.cheats_enabled()) {
                    echo(
                        "force_spawn: cheats are off".into(),
                        &mut console,
                        &mut line,
                    );
                    continue;
                }
                let Some(inbox) = inbox.as_deref_mut() else {
                    echo(
                        "force_spawn: no action inbox (not a listen host)".into(),
                        &mut console,
                        &mut line,
                    );
                    continue;
                };
                let request_id = seq.allocate();
                if let Err(error) =
                    inbox.push(local.0, ClientAction::ForceSpawn { request_id, pick })
                {
                    echo(format!("force_spawn: {error}"), &mut console, &mut line);
                    continue;
                }
                if !cmd.background {
                    let life = authority
                        .as_ref()
                        .and_then(|a| a.0.client_meta(local.0))
                        .map(|m| m.life_sequence)
                        .unwrap_or_default();
                    dispatch.wait_alive = Some((local.0, life));
                    dispatch.wait_alive_elapsed = 0.0;
                }
                echo(
                    format!("force_spawn: queued {pick:?} request_id={request_id}"),
                    &mut console,
                    &mut line,
                );
            }
            "damage" => {
                let amount = match cmd.args.first() {
                    None => Ok(40),
                    Some(raw) => raw
                        .parse::<i32>()
                        .map_err(|_| format!("usage: damage [amount] (got `{raw}`)")),
                };
                let amount = match amount {
                    Ok(n) if n > 0 => n,
                    Ok(_) => {
                        echo("damage: amount must be > 0".into(), &mut console, &mut line);
                        continue;
                    }
                    Err(msg) => {
                        echo(msg, &mut console, &mut line);
                        continue;
                    }
                };
                if !alive(&presented, local.0) {
                    echo(
                        "damage: not Alive — spawn a class first".into(),
                        &mut console,
                        &mut line,
                    );
                    continue;
                }
                if authority.as_ref().is_some_and(|a| !a.0.cheats_enabled()) {
                    echo("damage: cheats are off".into(), &mut console, &mut line);
                    continue;
                }
                let Some(inbox) = inbox.as_deref_mut() else {
                    echo(
                        "damage: no action inbox (not a listen host)".into(),
                        &mut console,
                        &mut line,
                    );
                    continue;
                };
                let request_id = seq.allocate();
                if let Err(error) =
                    inbox.push(local.0, ClientAction::DebugDamage { request_id, amount })
                {
                    echo(format!("damage: {error}"), &mut console, &mut line);
                    continue;
                }
                echo(
                    format!("damage: queued {amount} request_id={request_id}"),
                    &mut console,
                    &mut line,
                );
            }
            "nudge" => {
                if cmd.args.len() != 3 {
                    echo(
                        "usage: nudge <dx> <dy> <dz>".into(),
                        &mut console,
                        &mut line,
                    );
                    continue;
                }
                if !alive(&presented, local.0) {
                    echo(
                        "nudge: not Alive — spawn a class first".into(),
                        &mut console,
                        &mut line,
                    );
                    continue;
                }
                if authority.as_ref().is_some_and(|a| !a.0.cheats_enabled()) {
                    echo("nudge: cheats are off".into(), &mut console, &mut line);
                    continue;
                }
                let parse = |s: &str| {
                    s.parse::<f32>()
                        .ok()
                        .filter(|v| v.is_finite())
                        .ok_or_else(|| format!("nudge: not a finite number `{s}`"))
                };
                let delta = match (
                    parse(&cmd.args[0]),
                    parse(&cmd.args[1]),
                    parse(&cmd.args[2]),
                ) {
                    (Ok(x), Ok(y), Ok(z)) => [x, y, z],
                    (Err(msg), _, _) | (_, Err(msg), _) | (_, _, Err(msg)) => {
                        echo(msg, &mut console, &mut line);
                        continue;
                    }
                };
                let Some(authority) = authority.as_deref_mut() else {
                    echo(
                        "nudge: no authority world (not a listen host)".into(),
                        &mut console,
                        &mut line,
                    );
                    continue;
                };
                authority.0.gate_nudge_origin(local.0, delta);
                echo(
                    format!(
                        "nudge: authority origin += ({:.1} {:.1} {:.1})",
                        delta[0], delta[1], delta[2]
                    ),
                    &mut console,
                    &mut line,
                );
            }
            "name" => match cmd.args.as_slice() {
                [] => {
                    let shown = presented
                        .snapshot()
                        .and_then(|s| s.meta.for_client(local.0))
                        .and_then(|m| entity_iw4::client_state_name(&m.name).map(str::to_owned));
                    match shown {
                        Some(n) => echo(format!("name is \"{n}\""), &mut console, &mut line),
                        None => echo("name is \"\"".into(), &mut console, &mut line),
                    }
                }
                [raw] => {
                    let Some(inbox) = inbox.as_deref_mut() else {
                        echo(
                            "name: no action inbox (not a listen host)".into(),
                            &mut console,
                            &mut line,
                        );
                        continue;
                    };
                    let packed = entity_iw4::pack_client_state_name(raw);
                    let request_id = seq.allocate();
                    if let Err(error) = inbox.push(
                        local.0,
                        ClientAction::SetName {
                            request_id,
                            name: packed,
                        },
                    ) {
                        echo(format!("name: {error}"), &mut console, &mut line);
                        continue;
                    }
                    echo(
                        format!("name: queued `{raw}` request_id={request_id}"),
                        &mut console,
                        &mut line,
                    );
                }
                _ => echo("usage: name [string]".into(), &mut console, &mut line),
            },
            "force_match_start" => {
                if authority.as_ref().is_some_and(|a| !a.0.cheats_enabled()) {
                    echo(
                        "force_match_start: cheats are off".into(),
                        &mut console,
                        &mut line,
                    );
                    continue;
                }
                let Some(world) = authority.as_ref() else {
                    echo(
                        "force_match_start: no authority world (not a listen host)".into(),
                        &mut console,
                        &mut line,
                    );
                    continue;
                };
                match world.0.phase() {
                    MatchPhase::Playing => {
                        echo(
                            "force_match_start: already playing".into(),
                            &mut console,
                            &mut line,
                        );
                        continue;
                    }
                    MatchPhase::Warmup => {}
                    other => {
                        echo(
                            format!("force_match_start: match is not in warmup (phase={other:?})"),
                            &mut console,
                            &mut line,
                        );
                        continue;
                    }
                }
                let Some(inbox) = inbox.as_deref_mut() else {
                    echo(
                        "force_match_start: no action inbox (not a listen host)".into(),
                        &mut console,
                        &mut line,
                    );
                    continue;
                };
                let request_id = seq.allocate();
                if let Err(error) = inbox.push(
                    local.0,
                    ClientAction::SetMatchPhase {
                        request_id,
                        phase: MatchPhase::Playing,
                    },
                ) {
                    echo(
                        format!("force_match_start: {error}"),
                        &mut console,
                        &mut line,
                    );
                    continue;
                }
                if !cmd.background {
                    dispatch.wait_playing = true;
                    dispatch.wait_playing_elapsed = 0.0;
                    echo(
                        format!(
                            "force_match_start: queued SetMatchPhase Playing request_id={request_id} (sync until Playing)"
                        ),
                        &mut console,
                        &mut line,
                    );
                } else {
                    echo(
                        format!(
                            "force_match_start &: queued SetMatchPhase Playing request_id={request_id} (async)"
                        ),
                        &mut console,
                        &mut line,
                    );
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn update_showpos_overlay(
    debug_pos: Res<DebugPosOverlay>,
    skate: Res<frame::SkateMode>,
    presented: Res<PresentedSnapshot>,
    local: Res<LocalPresentClient>,
    clock: Res<net::AuthorityClock>,
    time: Res<Time>,
    actions: Res<net::ClientActionInput>,
    mut line: ResMut<ConsoleLine>,
    mut hud: Query<(&mut Text, &mut Visibility), With<ShowposHud>>,
) {
    if !debug_pos.0 || skate.rendering {
        for (_, mut vis) in &mut hud {
            *vis = Visibility::Hidden;
        }
        return;
    }
    let text = format_showpos_live(&presented, local.0, clock.tick, &time, &actions);
    line.0 = text.clone();
    for (mut hud_text, mut vis) in &mut hud {
        *vis = Visibility::Visible;
        **hud_text = text.clone();
    }
}

fn alive(presented: &PresentedSnapshot, id: sim::ClientId) -> bool {
    presented
        .snapshot()
        .and_then(|s| s.meta.for_client(id))
        .is_some_and(|m| m.lifecycle == ClientLifecycle::Alive)
}

fn format_showpos(presented: &PresentedSnapshot, id: sim::ClientId) -> String {
    match presented.alive_player(id) {
        Some(ps) => {
            let eye_z = ps.origin[2] + ps.view_height_current;
            format!(
                "showpos origin={:.1} {:.1} {:.1}  eye={:.1} {:.1} {:.1}  yaw={:.0} pitch={:.0}",
                ps.origin[0],
                ps.origin[1],
                ps.origin[2],
                ps.origin[0],
                ps.origin[1],
                eye_z,
                ps.viewangles[1],
                ps.viewangles[0]
            )
        }
        None => "showpos: not Alive".into(),
    }
}

fn format_showpos_live(
    presented: &PresentedSnapshot,
    id: sim::ClientId,
    tick: u32,
    time: &Time,
    actions: &net::ClientActionInput,
) -> String {
    let life = presented
        .snapshot()
        .and_then(|s| s.meta.for_client(id))
        .map(|m| format!("{:?}", m.lifecycle))
        .unwrap_or_else(|| "—".into());
    let (origin, eye, yaw, pitch, vz, ground) = match presented.alive_player(id) {
        Some(ps) => {
            let eye_z = ps.origin[2] + ps.view_height_current;
            (
                format!(
                    "{:.0} {:.0} {:.0}",
                    ps.origin[0], ps.origin[1], ps.origin[2]
                ),
                format!("{:.0} {:.0} {:.0}", ps.origin[0], ps.origin[1], eye_z),
                ps.viewangles[1],
                ps.viewangles[0],
                ps.velocity[2],
                ps.ground_entity_num,
            )
        }
        None => ("—".into(), "—".into(), 0.0, 0.0, 0.0, -1),
    };
    let fps = if time.delta_secs() > 0.0 {
        1.0 / time.delta_secs()
    } else {
        0.0
    };
    let mut held_names = Vec::new();
    actions.client.kb.visit_active_names(|n| held_names.push(n));
    let held = if held_names.is_empty() {
        "—".into()
    } else {
        held_names.join(",")
    };
    format!(
        "showpos tick={tick} life={life} origin={origin} eye={eye} yaw={yaw:.0} pitch={pitch:.0} vz={vz:.0} ground={ground} held=[{held}] fps={fps:.0}"
    )
}

fn parse_move(
    args: &[String],
    presented: &PresentedSnapshot,
    id: sim::ClientId,
) -> Result<([f32; 3], [f32; 3]), String> {
    if args.len() < 3 || args.len() > 5 {
        return Err("usage: move <x> <y> <z> [yaw] [pitch]".into());
    }
    let parse = |s: &str| {
        s.parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| format!("move: not a finite number `{s}`"))
    };
    let origin = [parse(&args[0])?, parse(&args[1])?, parse(&args[2])?];
    let current = presented
        .alive_player(id)
        .map(|ps| ps.viewangles)
        .unwrap_or([0.0, 0.0, 0.0]);
    let mut angles = current;
    if args.len() >= 4 {
        angles[1] = parse(&args[3])?;
    }
    if args.len() == 5 {
        angles[0] = parse(&args[4])?;
    }
    Ok((origin, angles))
}

fn parse_look(
    args: &[String],
    presented: &PresentedSnapshot,
    id: sim::ClientId,
) -> Result<([f32; 3], [f32; 3]), String> {
    if args.len() != 2 {
        return Err("usage: look <yaw> <pitch>".into());
    }
    let parse = |s: &str| {
        s.parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| format!("look: not a finite number `{s}`"))
    };
    let Some(ps) = presented.alive_player(id) else {
        return Err("look: not Alive — spawn a class first".into());
    };
    let mut angles = ps.viewangles;
    angles[1] = parse(&args[0])?;
    angles[0] = parse(&args[1])?;
    Ok((ps.origin, angles))
}

fn arm_wait_move(
    dispatch: &mut ConsoleDispatch,
    background: bool,
    client: sim::ClientId,
    origin: [f32; 3],
    angles: [f32; 3],
    verb: &str,
) {
    if background {
        diag::info!(Console, "{verb} &: async — FIFO not blocked");
        return;
    }
    dispatch.wait_move = Some(WaitMovePose {
        client,
        origin,
        angles,
    });
    dispatch.wait_move_elapsed = 0.0;
}

pub(crate) fn presented_matches_move(presented: &PresentedSnapshot, pose: WaitMovePose) -> bool {
    let Some(ps) = presented.alive_player(pose.client) else {
        return false;
    };
    pose_matches_move(ps.origin, ps.viewangles, pose.origin, pose.angles)
}

fn pose_matches_move(
    have_origin: [f32; 3],
    have_angles: [f32; 3],
    want_origin: [f32; 3],
    want_angles: [f32; 3],
) -> bool {
    (have_origin[0] - want_origin[0]).abs() < 1.0
        && (have_origin[1] - want_origin[1]).abs() < 1.0
        && angle_abs_delta(have_angles[0], want_angles[0]) < 0.5
        && angle_abs_delta(have_angles[1], want_angles[1]) < 0.5
}

fn angle_abs_delta(a: f32, b: f32) -> f32 {
    let mut d = (a - b) % 360.0;
    if d > 180.0 {
        d -= 360.0;
    } else if d < -180.0 {
        d += 360.0;
    }
    d.abs()
}

fn parse_force_spawn(args: &[String]) -> Result<SpawnPick, String> {
    const USAGE: &str = "usage: force_spawn [random <seed> | at <x> <y> <z> [yaw]]";
    let num = |raw: &String| {
        raw.parse::<f32>()
            .map_err(|_| format!("{USAGE} (got `{raw}`)"))
    };
    match args.first().map(String::as_str) {
        None => Ok(SpawnPick::Seeded(0)),
        Some("random") => {
            let [_, seed] = args else {
                return Err(USAGE.into());
            };
            seed.parse::<u64>()
                .map(SpawnPick::Seeded)
                .map_err(|_| format!("{USAGE} (got `{seed}`)"))
        }
        Some("at") if matches!(args.len(), 4 | 5) => Ok(SpawnPick::At {
            origin: [num(&args[1])?, num(&args[2])?, num(&args[3])?],
            yaw: args.get(4).map(num).transpose()?.unwrap_or(0.0),
        }),
        Some(_) => Err(USAGE.into()),
    }
}

pub(crate) fn update_skate_overlay(mode:Res<frame::SkateMode>,mut hud:Query<(&mut Text,&mut Visibility),With<SkateHud>>) {
    for (mut text,mut visibility) in &mut hud {
        let failed = !mode.preloaded && !mode.preload_pending && !mode.status.is_empty();
        let visible = !mode.hide_hud && !mode.rendering && (mode.active || mode.entering || failed);
        show(&mut visibility, visible);
        // Hidden through ordinary MW2 play: don't rebuild (and re-shape) a
        // string nobody can see every frame.
        if !visible {
            continue;
        }
        let want: String=if failed {format!("Skate unavailable: {}", mode.status)}
        else if mode.replaying {
            let state = if mode.replay_paused { "PAUSED" } else { "PLAYING" };
            let fov = if mode.replay_fov > 1.0 { mode.replay_fov } else { 55.0 };
            let roll = mode.replay_roll.to_degrees();
            let ramps = match mode.speed_ramps.len() {
                0 => String::new(),
                1 => "   1 speed ramp".to_string(),
                n => format!("   {n} speed ramps"),
            };
            let cam_label = match mode.active_cut_cam {
                Some(c) => format!(
                    "{}  (cut to {})",
                    frame::replay_cam_label(mode.replay_cam),
                    frame::replay_cam_label(c)
                ),
                None => frame::replay_cam_label(mode.replay_cam).to_string(),
            };
            let control = if frame::replay_cam_steerable(mode.eff_cam) {
                if mode.cam_control {
                    "\nCAMERA CONTROL - sticks move the camera"
                } else {
                    "\nPLAYBACK CONTROL - RS click to move the camera"
                }
            } else {
                ""
            };
            let head = format!(
                "SKATE REPLAY  [{state}]\n{cam_label}   {speed}x   fov {fov:.0}   roll {roll:+.0}{ramps}{control}",
                speed = mode.replay_speed,
            );
            // Prompts follow the device used last.
            let pad = mode.pad_prompts;
            if mode.help_expanded {
                let body = if pad { REPLAY_HELP_PAD } else { REPLAY_HELP_KEYS };
                let close = if pad { "Back" } else { "/" };
                format!("{head}\n\n{body}\n\n{close}: close")
            } else {
                head
            }
        }
        else if mode.recording {"SKATE | RECORDING\nBack: stop recording".into()}
        else if mode.entering && !mode.preloaded {"Skate is finishing map preparation... | J: cancel".into()}
        else if mode.controller.is_none() {"SKATE | Connect a controller | J: return to MW2".into()}
        else {format!("SKATE | skin: {}\nU/I: skin  Back: record  LB+Back: replay  RB+Back: replay last 45 s  Start: pause  J: exit", current_skin_name())};
        set_text(&mut text, &want);
    }
}
