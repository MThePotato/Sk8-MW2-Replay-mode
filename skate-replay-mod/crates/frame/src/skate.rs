use bevy::prelude::*;

/// Local skating presentation. Physics remains owned by the optional Skate host.
#[derive(Resource, Default)]
pub struct SkateMode {
    pub active: bool,
    pub entering: bool,
    pub preloaded: bool,
    pub preload_pending: bool,
    pub controller: Option<usize>,
    pub toggle_requested: bool,
    pub input_blocked: bool,
    pub record_requested: bool,
    pub replay_requested: bool,
    pub recording: bool,
    pub replaying: bool,
    pub save_requested: Option<String>,
    pub load_requested: Option<String>,
    pub cam_requested: bool,
    /// Set a specific replay camera (console `skate cam <n|name>`).
    pub cam_set_requested: Option<u8>,
    /// Capture the free camera as a keyframe (console `skate keyframe`).
    pub keyframe_add_requested: bool,
    /// Explicit keyframe time; None captures at the playhead.
    pub keyframe_add_time: Option<f32>,
    pub keyframe_clear_requested: bool,
    /// Add a camera cut at the playhead (console `skate cut`).
    pub cut_add_requested: bool,
    /// Hard-cut to a specific camera (console `skate cut <n|name>`).
    pub cut_to_requested: Option<u8>,
    pub cut_clear_requested: bool,
    /// Some(true) = eased/centripetal spline, Some(false) = linear lerp.
    pub ease_requested: Option<bool>,
    pub replay_cam: u8,
    pub replay_speed: f32,
    pub replay_paused: bool,
    pub free_cam_pos: Vec3,
    pub free_cam_yaw: f32,
    pub free_cam_pitch: f32,
    pub free_cam_fov: f32,
    pub replay_fov: f32,
    pub replay_roll: f32,
    pub hide_hud: bool,
    pub keyframe_times: Vec<f32>,
    pub speed_ramps: Vec<(f32, f32)>,
    /// Camera-cut markers for the timeline: (time, cam).
    pub cut_times: Vec<(f32, u8)>,
    /// The camera a cut is currently overriding to, if any.
    pub active_cut_cam: Option<u8>,
    pub help_expanded: bool,
    pub replay_duration: f32,
    pub replay_clock: f32,
    pub render_requested: bool,
    pub render_fps: f32,
    pub render_shutter: u32,
    pub render_crf: u32,
    pub render_scale: String,
    pub render_name: String,
    pub rendering: bool,
    /// Replay editor menu (Start / Tab). The skate adapter owns its logic;
    /// the console only draws it and reports mouse input.
    pub menu_open: bool,
    pub menu_focus: usize,
    /// Value text per `REPLAY_MENU` row, refreshed while the menu is open.
    pub menu_values: Vec<String>,
    /// Mouse intent from the menu: (row, 0 = activate, -1/+1 = adjust).
    pub menu_click: Option<(usize, i8)>,
    /// Row the mouse is holding down, for hold-to-confirm rows.
    pub menu_mouse_hold: Option<usize>,
    /// Short on-screen message and the seconds it has left.
    pub toast: Option<(String, f32)>,
    /// Hold-to-confirm in progress: what it does and 0..1 progress.
    pub hold: Option<(String, f32)>,
    /// The last replay input came from a controller (drives the prompts).
    pub pad_prompts: bool,
    pub trim_start: f32,
    pub trim_end: f32,
    /// TikTok template: a 9:16 guide frame on screen, and renders crop to it.
    pub portrait: bool,
    /// Cinematic template: 2.39:1 letterbox bars on screen, baked into renders.
    pub cinematic: bool,
    /// Camera control: in the free, body and board cameras the left stick and
    /// triggers move the camera instead of scrubbing and changing speed.
    /// Switches on when one of those cameras comes on screen; RS click toggles.
    pub cam_control: bool,
    /// Camera wheel (hold X): the camera highlighted while it is open.
    pub cam_wheel: Option<u8>,
    /// The camera under the playhead, as the controls see it this frame.
    pub eff_cam: u8,
    /// RB held long enough to show the quick-edit layer.
    pub quick_layer: bool,
    /// The marker a hold-B delete would remove: (time, description).
    pub delete_target: Option<(f32, String)>,
    /// Context button prompts for the prompt bar: (button, action).
    pub prompts: Vec<(String, String)>,
    /// Undo / redo steps available.
    pub undo_len: usize,
    pub redo_len: usize,
    /// Mouse input on the replay timeline this frame, in order.
    pub timeline_mouse: Vec<TimelineMouse>,
    /// A marker picked up to move: its time and description.
    pub grabbed: Option<(f32, String)>,
    /// Load-replay list, opened from the editor menu.
    pub load_open: bool,
    /// Saved replays, newest first: (name, "12.4 s · 5 min ago").
    pub load_entries: Vec<(String, String)>,
    pub load_focus: usize,
    /// A row of the load list clicked with the mouse.
    pub load_click: Option<usize>,
    /// Replay the last seconds of skating without having pressed record.
    pub instant_requested: bool,
    pub client: u32,
    pub root: Mat4,
    pub bones: Vec<Mat4>,
    pub names: Vec<String>,
    pub camera: Option<(Transform, f32)>,
    pub tick: u64,
    pub status: String,
}

/// What the mouse pressed on the replay timeline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimelineHit {
    /// Empty track: scrub.
    Track,
    TrimIn,
    TrimOut,
    Cut(usize),
    Keyframe(usize),
    Ramp(usize),
}

/// Mouse input on the replay timeline; times are clip seconds.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TimelineMouse {
    Press(TimelineHit, f32),
    Drag(f32),
    Release,
}

/// How a replay editor menu row responds to input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReplayMenuKind {
    /// A / Enter / click runs it.
    Action,
    /// Left/right change a value.
    Adjust,
    /// Destructive: needs A / Enter / the mouse held down.
    Hold,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReplayMenuItem {
    TrimIn,
    TrimOut,
    TrimClear,
    Speed,
    RampAdd,
    RampsClear,
    KeyframesClear,
    CutsClear,
    Undo,
    Redo,
    Fov,
    Roll,
    Easing,
    Skin,
    Fps,
    Blur,
    Resolution,
    Quality,
    Portrait,
    Cinematic,
    Save,
    Load,
    Render,
}

/// Replay editor menu rows, top to bottom.
pub const REPLAY_MENU: &[ReplayMenuItem] = &[
    ReplayMenuItem::TrimIn,
    ReplayMenuItem::TrimOut,
    ReplayMenuItem::TrimClear,
    ReplayMenuItem::Speed,
    ReplayMenuItem::RampAdd,
    ReplayMenuItem::RampsClear,
    ReplayMenuItem::KeyframesClear,
    ReplayMenuItem::CutsClear,
    ReplayMenuItem::Undo,
    ReplayMenuItem::Redo,
    ReplayMenuItem::Fov,
    ReplayMenuItem::Roll,
    ReplayMenuItem::Easing,
    ReplayMenuItem::Skin,
    ReplayMenuItem::Fps,
    ReplayMenuItem::Blur,
    ReplayMenuItem::Resolution,
    ReplayMenuItem::Quality,
    ReplayMenuItem::Portrait,
    ReplayMenuItem::Cinematic,
    ReplayMenuItem::Save,
    ReplayMenuItem::Load,
    ReplayMenuItem::Render,
];

impl ReplayMenuItem {
    pub fn label(self) -> &'static str {
        match self {
            Self::TrimIn => "Trim in",
            Self::TrimOut => "Trim out",
            Self::TrimClear => "Clear trim",
            Self::Speed => "Playback speed",
            Self::RampAdd => "Add speed ramp",
            Self::RampsClear => "Clear speed ramps",
            Self::KeyframesClear => "Clear keyframes",
            Self::CutsClear => "Clear cuts",
            Self::Undo => "Undo",
            Self::Redo => "Redo",
            Self::Fov => "Field of view",
            Self::Roll => "Roll",
            Self::Easing => "Keyframe easing",
            Self::Skin => "Skater skin",
            Self::Fps => "Frame rate",
            Self::Blur => "Motion blur",
            Self::Resolution => "Resolution",
            Self::Quality => "Quality",
            Self::Portrait => "TikTok 9:16 frame",
            Self::Cinematic => "Cinematic bars (2.39:1)",
            Self::Save => "Save replay",
            Self::Load => "Load replay",
            Self::Render => "Render video",
        }
    }

    pub fn kind(self) -> ReplayMenuKind {
        match self {
            Self::Speed
            | Self::Fov
            | Self::Roll
            | Self::Easing
            | Self::Skin
            | Self::Fps
            | Self::Blur
            | Self::Resolution
            | Self::Quality
            | Self::Portrait
            | Self::Cinematic => ReplayMenuKind::Adjust,
            Self::RampsClear | Self::KeyframesClear | Self::CutsClear => ReplayMenuKind::Hold,
            _ => ReplayMenuKind::Action,
        }
    }

    /// One line under the menu explaining the focused row.
    pub fn description(self) -> &'static str {
        match self {
            Self::TrimIn => "The replay and the video start here (the playhead now)",
            Self::TrimOut => "The replay and the video end here (the playhead now)",
            Self::TrimClear => "Use the whole recording again",
            Self::Speed => "Speed of the whole clip, or of the next speed ramp you add",
            Self::RampAdd => "From the playhead, ease into the speed above",
            Self::RampsClear => "Remove every speed ramp. Hold to confirm",
            Self::KeyframesClear => "Remove every camera keyframe. Hold to confirm",
            Self::CutsClear => "Remove every camera cut. Hold to confirm",
            Self::Undo => "Take back the last edit to cuts, keyframes, ramps or trim",
            Self::Redo => "Put back the last edit you undid",
            Self::Fov => "Zoom: lower is tighter, higher is wider",
            Self::Roll => "Tilt the camera for a dutch angle",
            Self::Easing => "Smooth curves or straight lines between keyframes",
            Self::Skin => "The skater's outfit in the replay and the video",
            Self::Fps => "Frames per second of the rendered video",
            Self::Blur => "Frames blended per video frame. Higher is smoother motion",
            Self::Resolution => "Size of the rendered video",
            Self::Quality => "Lower CRF is sharper but a bigger file",
            Self::Portrait => "Vertical 9:16 crop for TikTok, Shorts and Reels",
            Self::Cinematic => "Widescreen black bars, baked into the video",
            Self::Save => "Save the clip with its cuts, keyframes, ramps and trim",
            Self::Load => "Open a replay you saved earlier",
            Self::Render => "Write the trimmed replay to an MP4 video, as it plays here",
        }
    }

    /// Section heading drawn above this row, if it starts one.
    pub fn section(self) -> Option<&'static str> {
        match self {
            Self::TrimIn => Some("EDIT"),
            Self::Fov => Some("CAMERA"),
            Self::Fps => Some("RENDER"),
            _ => None,
        }
    }
}

/// Number of replay cameras.
pub const REPLAY_CAM_COUNT: u8 = 14;

/// Replay camera names as shown on screen. The console and log keep the short
/// command names (`skate cam firstperson`); this is what a player reads.
pub fn replay_cam_label(cam: u8) -> &'static str {
    match cam {
        0 => "Recorded",
        1 => "Orbit",
        2 => "Left side",
        3 => "Right side",
        4 => "Top down",
        5 => "Chase",
        6 => "Free cam",
        7 => "Keyframes",
        8 => "First person",
        9 => "Fisheye",
        10 => "Tripod",
        11 => "Follow",
        12 => "Body cam",
        _ => "Board cam",
    }
}

/// Cameras the player steers with the sticks: free, body and board.
pub fn replay_cam_steerable(cam: u8) -> bool {
    matches!(cam, 6 | 12 | 13)
}
