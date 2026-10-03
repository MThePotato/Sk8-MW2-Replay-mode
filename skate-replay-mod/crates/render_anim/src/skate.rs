//! Local Skate gameplay adapter. Rendering and match ownership remain in IW4L.
pub mod collision;
pub mod rails;
pub mod rig;
use bevy::input::gamepad::{Gamepad, GamepadRumbleIntensity, GamepadRumbleRequest};
use bevy::input::mouse::MouseWheel;
use bevy::input::{ButtonInput, keyboard::KeyCode};
use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured};
use frame::{AppScreen, REPLAY_MENU, ReplayMenuItem, ReplayMenuKind, SkateMode, TimelineHit, TimelineMouse};
use skate_host::bridge::{CollisionBuilder, InputFrame, Pose, PreparedCollision, Session};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

enum Job {
    Activate(u64, Vec3, f32, f32),
    Step(u64, f32, InputFrame, f32),
    Suspend,
}
enum Reply {
    Ready,
    Activated(u64, Pose, u128),
    Pose(u64, Pose),
    Error(String),
}
struct RecordedFrame {
    pose: Pose,
    time_secs: f32,
}

#[derive(Clone, PartialEq)]
struct Keyframe {
    time: f32,
    pos: Vec3,
    yaw: f32,
    pitch: f32,
    fov: f32,
}

#[derive(Clone, PartialEq)]
struct SpeedRamp {
    time: f32,
    speed: f32,
}

/// A camera cut: at `time` the replay camera switches to `cam`.
#[derive(Clone, PartialEq)]
struct CameraCut {
    time: f32,
    cam: u8,
    /// Where the free cam sits for this cut. Each free-cam cut keeps its own
    /// shot, so playing into the cut shows the angle you set up instead of
    /// carrying on from the previous camera.
    free: Option<FreePose>,
}

/// A placed free-cam shot.
#[derive(Clone, Copy, PartialEq, Debug)]
struct FreePose {
    pos: Vec3,
    yaw: f32,
    pitch: f32,
    fov: f32,
}

fn free_pose(mode: &SkateMode) -> FreePose {
    FreePose {
        pos: mode.free_cam_pos,
        yaw: mode.free_cam_yaw,
        pitch: mode.free_cam_pitch,
        fov: mode.free_cam_fov,
    }
}

fn set_free_pose(mode: &mut SkateMode, p: FreePose) {
    mode.free_cam_pos = p.pos;
    mode.free_cam_yaw = p.yaw;
    mode.free_cam_pitch = p.pitch;
    mode.free_cam_fov = p.fov;
}

/// The cut showing at `t`, by index.
fn active_cut_index(cuts: &[CameraCut], t: f32) -> Option<usize> {
    cuts.iter().rposition(|c| c.time <= t)
}

/// The free-cam shot slot at `t`: the active cut's, or the base camera's
/// when no cut covers the playhead. Keyed by the cut's time so neighbouring
/// free-cam cuts are told apart.
fn free_slot_key(cuts: &[CameraCut], t: f32) -> f32 {
    active_cut_index(cuts, t).map(|i| cuts[i].time).unwrap_or(-1.0)
}

fn free_slot(host: &Host, t: f32) -> Option<FreePose> {
    match active_cut_index(&host.cuts, t) {
        Some(i) => host.cuts[i].free,
        None => host.free_base,
    }
}

fn store_free_slot(host: &mut Host, t: f32, p: FreePose) {
    match active_cut_index(&host.cuts, t) {
        Some(i) => host.cuts[i].free = Some(p),
        None => host.free_base = Some(p),
    }
}

/// On entering a free-cam shot: restore its saved angle, or start it from
/// the view on screen if it has none yet.
fn enter_free_slot(host: &mut Host, mode: &mut SkateMode, t: f32, store: bool) {
    if let Some(p) = free_slot(host, t) {
        set_free_pose(mode, p);
    } else {
        init_free_cam(mode, host.last_view);
        if store {
            let p = free_pose(mode);
            store_free_slot(host, t, p);
        }
    }
}

/// On entering a tripod shot: stand the tripod where that cut saved it, or
/// where the camera on screen is now, and save that into the cut. Keeping
/// the spot per cut (and in the saved file) makes the render put the tripod
/// exactly where the preview did, instead of wherever tripod was first used.
fn enter_tripod_slot(host: &mut Host, mode: &SkateMode, t: f32, store: bool) {
    let pos = match free_slot(host, t) {
        Some(p) => p.pos,
        None => {
            let Some((eye, fov)) = host.last_view.or(mode.camera) else {
                return;
            };
            let (yaw, pitch) = yaw_pitch_from_forward(*eye.forward());
            if store {
                store_free_slot(host, t, FreePose { pos: eye.translation, yaw, pitch, fov });
            }
            eye.translation
        }
    };
    host.tripod_pos = pos;
    host.tripod_init = true;
}

/// Runs when the camera under the playhead changes to a free-cam or tripod
/// shot (or between two such cuts), so each shot restores its own placement.
fn enter_placed_shot(host: &mut Host, mode: &mut SkateMode, t: f32, eff_cam: u8, store: bool) {
    let key = free_slot_key(&host.cuts, t);
    let entering = eff_cam != host.prev_eff_cam || key != host.prev_free_key;
    if entering {
        match eff_cam {
            6 => enter_free_slot(host, mode, t, store),
            10 => enter_tripod_slot(host, mode, t, store),
            _ => {}
        }
    }
    host.prev_eff_cam = eff_cam;
    host.prev_free_key = key;
}

/// A finished background save: the clip name and where it went.
type SaveResult = (String, Result<PathBuf, String>);

#[derive(Resource, Default)]
struct Host {
    send: Option<mpsc::Sender<Job>>,
    receive: Option<Mutex<mpsc::Receiver<Reply>>>,
    clip: Option<Arc<asset_world::ClipCollision>>,
    ready: bool,
    enter_requested: bool,
    activating: bool,
    epoch: u64,
    pad_packet: u32,
    previous_buttons: u16,
    input_suspended: bool,
    logged_tick: u64,
    recorded: Vec<RecordedFrame>,
    /// Recording clock. Like the instant-replay clock it only advances by a
    /// frame's worth per pose, so pausing mid-recording leaves no dead air.
    record_clock: f32,
    record_last: f32,
    /// Scratch pose blended between two recorded samples each replay frame.
    blend: Option<Pose>,
    /// A save writing on a worker thread: (name, result).
    saving: Option<Mutex<mpsc::Receiver<SaveResult>>>,
    /// The demo skeleton's bone names, kept once instead of per frame.
    names: Vec<String>,
    replay_index: usize,
    replay_clock_secs: f32,
    keyframes: Vec<Keyframe>,
    speed_ramps: Vec<SpeedRamp>,
    cuts: Vec<CameraCut>,
    /// Keyframes lerp in straight lines instead of the eased spline. Stored
    /// inverted so the derived default is the smooth spline.
    keyframe_linear: bool,
    follow_eye: Vec3,
    follow_init: bool,
    tripod_pos: Vec3,
    tripod_init: bool,
    /// Aim and dolly of the body- and board-locked cameras.
    locked_yaw: f32,
    locked_pitch: f32,
    locked_dist: f32,
    /// Set while a replay or render has taken the MW2 HUD off screen.
    hud_suppressed: bool,
    /// Throttle for the free-cam input diagnostic.
    free_cam_log: f32,
    /// Replay controls: holds, auto-repeat, scrub snapping and the camera
    /// state the free cam and keyframe capture start from.
    hold_exit: Hold,
    hold_delete: Hold,
    hold_menu: Hold,
    /// Back was pressed during this replay (the press that started it can't
    /// count as a tap or an exit hold).
    back_armed: bool,
    rep_h: Repeat,
    rep_v: Repeat,
    scrub_sticky: f32,
    last_view: Option<(Transform, f32)>,
    prev_eff_cam: u8,
    /// Free-cam shot of the base camera (no cut over the playhead).
    free_base: Option<FreePose>,
    /// Which free-cam shot was on screen last frame (`free_slot_key`).
    prev_free_key: f32,
    /// Seconds since the free cam last moved: a flight is one undo step.
    free_edit_secs: f32,
    /// X held on the pad: seconds, and whether the camera wheel opened.
    x_down: bool,
    x_secs: f32,
    /// RB held on the pad: seconds, and whether a quick-edit action used it
    /// (then releasing it is not a camera step).
    rb_down: bool,
    rb_secs: f32,
    rb_used: bool,
    /// Both sticks were clicked together (the skating toggle), so neither
    /// release counts as a single click.
    stick_combo: bool,
    /// Hold B during a render to cancel it from the pad.
    hold_render_cancel: Hold,
    /// Undo / redo history of timeline edits.
    undo: Vec<EditSnapshot>,
    redo: Vec<EditSnapshot>,
    /// Set by undo / redo so their own change isn't recorded as a new edit.
    history_applied: bool,
    /// The first replay of the session shows a tip.
    tip_shown: bool,
    /// B stays blocked from deleting until it is released (it was just used
    /// to cancel the camera wheel or to undo).
    b_block: bool,
    /// Seconds B has been down, to tell a tap (pick up / drop a marker) from
    /// a hold (delete).
    b_secs: f32,
    /// A marker picked up to move: it follows the playhead until dropped.
    grab: Option<Marker>,
    /// What the mouse is dragging on the timeline.
    mouse_drag: Option<TimelineHit>,
    /// Timeline state when a drag or a marker move began: the whole move is
    /// one undo step.
    drag_base: Option<EditSnapshot>,
    /// Instant replay: the last `INSTANT_SECS` of skating, always recording.
    rolling: VecDeque<RecordedFrame>,
    /// Instant-replay clock; pauses and menus don't leave gaps in it.
    rolling_clock: f32,
    rolling_last: f32,
}

/// Seconds of skating kept for an instant replay.
const INSTANT_SECS: f32 = 45.0;
/// Seconds a speed ramp takes to ease into its new speed.
const RAMP_BLEND: f32 = 0.25;

/// Everything an edit can change on the timeline, for undo / redo.
#[derive(Clone, PartialEq)]
struct EditSnapshot {
    keyframes: Vec<Keyframe>,
    cuts: Vec<CameraCut>,
    speed_ramps: Vec<SpeedRamp>,
    trim_start: f32,
    trim_end: f32,
    portrait: bool,
    cinematic: bool,
}

const UNDO_LIMIT: usize = 64;

fn edit_snapshot(host: &Host, mode: &SkateMode) -> EditSnapshot {
    EditSnapshot {
        keyframes: host.keyframes.clone(),
        cuts: host.cuts.clone(),
        speed_ramps: host.speed_ramps.clone(),
        trim_start: mode.trim_start,
        trim_end: mode.trim_end,
        portrait: mode.portrait,
        cinematic: mode.cinematic,
    }
}

fn apply_snapshot(host: &mut Host, mode: &mut SkateMode, s: EditSnapshot) {
    host.keyframes = s.keyframes;
    host.cuts = s.cuts;
    host.speed_ramps = s.speed_ramps;
    mode.trim_start = s.trim_start;
    mode.trim_end = s.trim_end;
    mode.portrait = s.portrait;
    mode.cinematic = s.cinematic;
}

fn undo_edit(host: &mut Host, mode: &mut SkateMode) {
    let Some(prev) = host.undo.pop() else {
        toast(mode, "Nothing to undo");
        return;
    };
    let now = edit_snapshot(host, mode);
    host.redo.push(now);
    apply_snapshot(host, mode, prev);
    host.history_applied = true;
    host.grab = None;
    host.drag_base = None;
    // Re-enter the free-cam shot so an undone camera move shows at once
    // (and isn't saved straight back over by the live free cam).
    host.prev_eff_cam = u8::MAX;
    toast(mode, format!("Undone ({} more)", host.undo.len()));
}

fn redo_edit(host: &mut Host, mode: &mut SkateMode) {
    let Some(next) = host.redo.pop() else {
        toast(mode, "Nothing to redo");
        return;
    };
    let now = edit_snapshot(host, mode);
    host.undo.push(now);
    apply_snapshot(host, mode, next);
    host.history_applied = true;
    host.grab = None;
    host.drag_base = None;
    // Re-enter the free-cam shot so an undone camera move shows at once
    // (and isn't saved straight back over by the live free cam).
    host.prev_eff_cam = u8::MAX;
    toast(mode, format!("Redone ({} more)", host.redo.len()));
}

fn clear_history(host: &mut Host) {
    host.undo.clear();
    host.redo.clear();
}

#[derive(Resource, Default)]
struct SkateRenderState {
    rendering: bool,
    capturing: bool,
    finished: bool,
    frame: u32,
    total: u32,
    dir: PathBuf,
    width: u32,
    height: u32,
    bgra: bool,
    pipe: Option<RenderPipe>,
    pipe_cfg: Option<RenderPipeConfig>,
    pipe_error: Option<String>,
}

pub fn register(app: &mut App) {
    app.init_resource::<SkateMode>()
        .init_resource::<Host>()
        .init_resource::<SkateRenderState>()
        .add_systems(Startup, preload_assets)
        .add_systems(
            Update,
            update
                .after(frame::PresentedPublished)
                .before(crate::sync_camera_from_presented)
                .before(render_scene::GfxSceneAdd)
                .in_set(frame::ClientSet::Present),
        );
}

fn preload_assets(mut mode: ResMut<SkateMode>) {
    let Some(root) = std::env::var_os("IW4L_SKATE_ASSETS") else {
        return;
    };
    mode.preload_pending = true;
    let skin_count = assets::bot_model::install_skins(std::path::Path::new(&root)).len();
    diag::info!(World, "skater skins: {skin_count} entries (incl. default)");
    if let Err(e) = std::thread::Builder::new()
        .name("skate-preload".into())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let start = std::time::Instant::now();
            match Session::preload(std::path::Path::new(&root)) {
                Ok(()) => diag::info!(
                    World,
                    "Skate animation banks preloaded in {}ms",
                    start.elapsed().as_millis()
                ),
                Err(e) => diag::warn!(World, "Skate preload: {e}"),
            }
        })
    {
        diag::warn!(World, "Skate preload thread: {e}");
    }
}

/// Blocks across either way of the skater that a Minecraft world's collision
/// covers, blocks up and down, and how far the skater goes before it is
/// rebuilt around them.
const BLOCK_RADIUS: i32 = 40;
const BLOCK_DEPTH: i32 = 20;
const BLOCK_RECENTRE: f32 = 14.0;

/// A Minecraft world's collision around map point `centre`, for Skate. Its
/// block edges become grind rails the same way any map's lips do: the same
/// detector that walks MW2 collision walks the block faces it is handed.
fn block_collision(
    builder: &CollisionBuilder,
    centre: Vec3,
) -> Result<(PreparedCollision, usize), String> {
    let map_triangles: Vec<[Vec3; 3]> =
        sim::voxel::collision_triangles(centre.to_array(), BLOCK_RADIUS, BLOCK_DEPTH)
            .into_iter()
            .map(|t| t.map(Vec3::from_array))
            .collect();
    if map_triangles.is_empty() {
        return Err("no blocks around the skater yet".into());
    }
    let (found, census) = rails::find(&map_triangles);
    diag::debug!(
        World,
        "Skate block rails: {} walkable edges, {} lips, {} runs, {} rails",
        census.candidates,
        census.lips,
        census.runs,
        census.rails,
    );
    let triangles: Vec<[[f32; 3]; 3]> = map_triangles
        .iter()
        .map(|t| t.map(|p| collision::to_skate(p).to_array()))
        .collect();
    let rails: Vec<Vec<[f32; 3]>> = found
        .into_iter()
        .map(|rail| {
            rail.into_iter()
                .map(|p| collision::to_skate(p).to_array())
                .collect()
        })
        .collect();
    let n = triangles.len();
    Ok((builder.build(triangles, rails)?, n))
}

/// One retained session per map. Leaving skating only pauses this worker;
/// collision, decoded animation banks, graphs and the rig remain resident.
fn preload_map(host: &mut Host, clip: Arc<asset_world::ClipCollision>) -> Result<(), String> {
    let root =
        std::env::var_os("IW4L_SKATE_ASSETS").ok_or("IW4L_SKATE_ASSETS is not configured")?;
    rig::reference().ok_or("Skate rig.json could not be loaded")?;
    assets::bot_model::local_skate_board().ok_or("Skate board.json could not be loaded")?;
    let (send, receive) = mpsc::channel();
    let (publish, results) = mpsc::channel();
    let geometry = clip.clone();
    std::thread::Builder::new()
        .name("iw4l-skate".into())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let result = (|| -> Result<(), String> {
                let start = std::time::Instant::now();
                let world = collision::extract(&geometry);
                let mut session = Session::new(
                    std::path::Path::new(&root),
                    world.triangles,
                    world.rails,
                    [0., 0., 0.],
                    0.,
                )?;
                diag::info!(
                    World,
                    "Skate map session preloaded in {}ms",
                    start.elapsed().as_millis()
                );
                if publish.send(Reply::Ready).is_err() {
                    return Ok(());
                }
                // On a Minecraft world the collision streams: built around
                // the skater off this thread and swapped in as they move or
                // the blocks change.
                let builder = session.collision_builder();
                let (build_send, build_jobs) = mpsc::channel::<Vec3>();
                let (built_send, built) = mpsc::channel::<(u64, Vec3, Result<(PreparedCollision, usize), String>)>();
                std::thread::Builder::new()
                    .name("iw4l-skate-blocks".into())
                    .spawn(move || {
                        while let Ok(mut centre) = build_jobs.recv() {
                            while let Ok(newer) = build_jobs.try_recv() {
                                centre = newer;
                            }
                            let revision = sim::voxel::revision();
                            let prepared = block_collision(&builder, centre);
                            if built_send.send((revision, centre, prepared)).is_err() {
                                break;
                            }
                        }
                    })
                    .map_err(|e| e.to_string())?;
                let mut blocks: Option<(u64, Vec3)> = None;
                let mut building = false;
                let mut requested = std::time::Instant::now();
                let mut skater_at: Option<Vec3> = None;
                let mut accumulated = 0.;
                let mut epoch = 0;
                while let Ok(job) = receive.recv() {
                    match job {
                        Job::Activate(new_epoch, spawn, yaw, aspect_ratio) => {
                            epoch = new_epoch;
                            accumulated = 0.;
                            let start = std::time::Instant::now();
                            session.set_aspect_ratio(aspect_ratio);
                            if sim::voxel::active() {
                                let revision = sim::voxel::revision();
                                match block_collision(&session.collision_builder(), spawn) {
                                    Ok((prepared, n)) => {
                                        session.install_collision(prepared)?;
                                        blocks = Some((revision, spawn));
                                        diag::info!(World, "Skate: {n} block collision triangles around the spawn");
                                    }
                                    Err(e) => diag::warn!(World, "Skate block collision: {e}"),
                                }
                                skater_at = Some(spawn);
                            }
                            let p = session.activate(
                                collision::to_skate(spawn).to_array(),
                                yaw.to_radians() + std::f32::consts::FRAC_PI_2,
                            )?;
                            if publish
                                .send(Reply::Activated(epoch, p, start.elapsed().as_millis()))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Job::Suspend => {
                            accumulated = 0.;
                            session.suspend_input();
                        }
                        Job::Step(request, dt, input, aspect_ratio) => {
                            if request != epoch {
                                continue;
                            }
                            if let Ok((revision, centre, prepared)) = built.try_recv() {
                                building = false;
                                match prepared {
                                    Ok((prepared, _)) => {
                                        session.install_collision(prepared)?;
                                        blocks = Some((revision, centre));
                                    }
                                    Err(e) => diag::warn!(World, "Skate block collision: {e}"),
                                }
                            }
                            if sim::voxel::active()
                                && !building
                                && let Some(at) = skater_at
                            {
                                let far = blocks.is_none_or(|(_, centre)| {
                                    let d = (at - centre) / sim::voxel::BLOCK;
                                    d.truncate().length() > BLOCK_RECENTRE || d.z.abs() > BLOCK_DEPTH as f32 * 0.5
                                });
                                // Only changes that reach the blocks it covers: chunks
                                // stream in and out far away the whole time.
                                let changed = requested.elapsed().as_secs_f32() > 0.25
                                    && blocks.is_some_and(|(revision, centre)| {
                                        sim::voxel::changed_near(revision, centre.to_array(), BLOCK_RADIUS, BLOCK_DEPTH)
                                    });
                                if (far || changed) && build_send.send(at).is_ok() {
                                    building = true;
                                    requested = std::time::Instant::now();
                                }
                            }
                            session.set_aspect_ratio(aspect_ratio);
                            session.collect(input, dt);
                            accumulated = (accumulated + dt).min(0.15);
                            let mut advanced = false;
                            // The native camera can change the simulation period.
                            while accumulated >= session.period() {
                                accumulated -= session.period();
                                session.advance()?;
                                advanced = true;
                            }
                            if advanced {
                                let p = session.pose();
                                if !p.root.is_finite() || p.bones.iter().any(|b| !b.is_finite()) {
                                    return Err("Skate published a non-finite pose".into());
                                }
                                skater_at = Some(collision::from_skate(p.root.w_axis.truncate()));
                                if publish.send(Reply::Pose(epoch, p)).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                let _ = publish.send(Reply::Error(e));
            }
        })
        .map_err(|e| e.to_string())?;
    host.send = Some(send);
    host.receive = Some(Mutex::new(results));
    host.clip = Some(clip);
    host.ready = false;
    Ok(())
}

fn stop(host: &mut Host, mode: &mut SkateMode, authority: &mut net::AuthorityWorld) {
    authority
        .0
        .set_external_motion(sim::ClientId(mode.client), false);
    host.enter_requested = false;
    host.activating = false;
    host.epoch = host.epoch.wrapping_add(1);
    if let Some(send) = &host.send {
        let _ = send.send(Job::Suspend);
    }
    mode.active = false;
    mode.entering = false;
    mode.recording = false;
    mode.replaying = false;
    mode.replay_paused = false;
    mode.menu_open = false;
    mode.load_open = false;
    mode.hold = None;
    mode.camera = None;
    mode.bones.clear();
    host.rolling.clear();
    host.grab = None;
    host.mouse_drag = None;
    mode.status.clear();
    diag::info!(World, "Skate mode stopped; map session retained");
}

/// Publishes one skater pose to the presentation. Reads the pose in place and
/// reuses `mode`'s buffers: replay playback calls this every frame, so no
/// per-frame allocation — least of all a clone of the bone-name list —
/// belongs here.
fn present(mode: &mut SkateMode, p: &Pose, names: &[String], authority: &mut net::AuthorityWorld) {
    let b = collision::basis();
    let mut root = b * p.root * b.inverse();
    root.w_axis = collision::from_skate(p.root.w_axis.truncate()).extend(1.);
    mode.root = root;
    mode.bones.clear();
    mode.bones.extend_from_slice(&p.bones);
    // The skeleton is constant for a demo: copy the names once, not per frame.
    if mode.names.len() != names.len() {
        mode.names = names.to_vec();
    }
    mode.tick = p.tick;
    mode.status.clear();
    mode.status.push_str(&p.state);
    mode.camera = p.camera.map(|(position, basis, fov)| {
        (
            Transform::from_translation(collision::from_skate(position)).looking_to(
                b.transform_vector3(basis.z_axis).normalize(),
                b.transform_vector3(basis.y_axis).normalize(),
            ),
            fov,
        )
    });
    authority.0.set_origin(
        sim::ClientId(mode.client),
        root.w_axis.truncate().to_array(),
    );
}

/// Moves `host.replay_index` to the recorded sample at or before `t`.
fn seek_index(host: &mut Host, t: f32) {
    while host.replay_index + 1 < host.recorded.len() && host.recorded[host.replay_index + 1].time_secs <= t {
        host.replay_index += 1;
    }
    while host.replay_index > 0 && host.recorded[host.replay_index].time_secs > t {
        host.replay_index -= 1;
    }
}

/// Larger gaps between two samples are a pause or a respawn, not motion:
/// those are shown as recorded instead of blended across.
const BLEND_MAX_GAP_SECS: f32 = 0.25;
const BLEND_MAX_JUMP: f32 = 2.0;

fn blend_mat(a: &Mat4, b: &Mat4, alpha: f32) -> Mat4 {
    let (sa, ra, ta) = a.to_scale_rotation_translation();
    let (sb, rb, tb) = b.to_scale_rotation_translation();
    let m = Mat4::from_scale_rotation_translation(sa.lerp(sb, alpha), ra.slerp(rb, alpha), ta.lerp(tb, alpha));
    if m.is_finite() { m } else { *a }
}

fn nlerp(a: Vec3, b: Vec3, alpha: f32) -> Vec3 {
    let v = a.lerp(b, alpha);
    if v.length_squared() > 1.0e-12 { v.normalize() } else { a }
}

/// Presents the skater at clip time `t`, blending the two recorded samples
/// around it. Skate publishes poses at its simulation rate, so without the
/// blend slow motion repeats each pose for several frames (it stutters) and
/// the render's motion blur averages identical frames (it does nothing).
fn present_at(mode: &mut SkateMode, host: &mut Host, t: f32, authority: &mut net::AuthorityWorld) {
    seek_index(host, t);
    let i = host.replay_index;
    let Some(a) = host.recorded.get(i) else {
        return;
    };
    let blendable = host.recorded.get(i + 1).and_then(|b| {
        let span = b.time_secs - a.time_secs;
        let alpha = (t - a.time_secs) / span;
        let jump = (b.pose.root.w_axis - a.pose.root.w_axis).truncate().length();
        (span > 1.0e-6
            && span <= BLEND_MAX_GAP_SECS
            && jump <= BLEND_MAX_JUMP
            && alpha > 1.0e-3
            && alpha < 1.0 - 1.0e-3
            && a.pose.bones.len() == b.pose.bones.len())
        .then_some((b, alpha))
    });
    let Some((b, alpha)) = blendable else {
        present(mode, &a.pose, &host.names, authority);
        return;
    };
    let out = host.blend.get_or_insert_with(|| Pose {
        root: Mat4::IDENTITY,
        bones: Vec::new(),
        names: Vec::new(),
        camera: None,
        velocity: Vec3::ZERO,
        tick: 0,
        state: String::new(),
    });
    let (a, b) = (&a.pose, &b.pose);
    out.root = blend_mat(&a.root, &b.root, alpha);
    out.bones.clear();
    out.bones
        .extend(a.bones.iter().zip(&b.bones).map(|(x, y)| blend_mat(x, y, alpha)));
    out.camera = match (a.camera, b.camera) {
        (Some((pa, ba, fa)), Some((pb, bb, fb))) => Some((
            pa.lerp(pb, alpha),
            Mat3::from_cols(
                nlerp(ba.x_axis, bb.x_axis, alpha),
                nlerp(ba.y_axis, bb.y_axis, alpha),
                nlerp(ba.z_axis, bb.z_axis, alpha),
            ),
            fa + (fb - fa) * alpha,
        )),
        (camera, _) => camera,
    };
    out.velocity = a.velocity.lerp(b.velocity, alpha);
    out.tick = a.tick;
    out.state.clear();
    out.state.push_str(&a.state);
    present(mode, out, &host.names, authority);
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DemoFile {
    /// Clip length in seconds. Written first so the load list can read it
    /// from the start of the file without decoding every frame.
    #[serde(default)]
    duration: f32,
    names: Vec<String>,
    frames: Vec<FrameData>,
    #[serde(default)]
    cuts: Vec<CutData>,
    #[serde(default)]
    keyframes: Vec<KeyframeData>,
    #[serde(default)]
    ramps: Vec<RampData>,
    #[serde(default = "default_ease")]
    ease: bool,
    #[serde(default)]
    trim_start: f32,
    #[serde(default)]
    trim_end: f32,
    /// Free-cam / tripod placement of the base camera (no cut over it).
    #[serde(default)]
    free_base: Option<([f32; 3], f32, f32, f32)>,
}

fn default_ease() -> bool {
    true
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CutData {
    time: f32,
    cam: u8,
    /// Free-cam shot: position, yaw, pitch, fov.
    #[serde(default)]
    free: Option<([f32; 3], f32, f32, f32)>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct KeyframeData {
    time: f32,
    pos: [f32; 3],
    yaw: f32,
    pitch: f32,
    fov: f32,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RampData {
    time: f32,
    speed: f32,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FrameData {
    root: [[f32; 3]; 4],
    bones: Vec<[[f32; 3]; 4]>,
    camera: Option<([f32; 3], [[f32; 3]; 3], f32)>,
    time: f32,
}

fn mat4_top3(m: &Mat4) -> [[f32; 3]; 4] {
    let c = m.to_cols_array_2d();
    [
        [c[0][0], c[0][1], c[0][2]],
        [c[1][0], c[1][1], c[1][2]],
        [c[2][0], c[2][1], c[2][2]],
        [c[3][0], c[3][1], c[3][2]],
    ]
}

fn mat4_from_top3(t: &[[f32; 3]; 4]) -> Mat4 {
    let mut m = [[0.0f32; 4]; 4];
    for col in 0..4 {
        m[col][0] = t[col][0];
        m[col][1] = t[col][1];
        m[col][2] = t[col][2];
        m[col][3] = 0.0;
    }
    m[3][3] = 1.0;
    Mat4::from_cols_array_2d(&m)
}

fn frame_to_data(f: &RecordedFrame) -> FrameData {
    let p = &f.pose;
    FrameData {
        root: mat4_top3(&p.root),
        bones: p.bones.iter().map(mat4_top3).collect(),
        camera: p
            .camera
            .map(|(pos, basis, fov)| (pos.to_array(), basis.to_cols_array_2d(), fov)),
        time: f.time_secs,
    }
}

fn data_to_frame(d: FrameData) -> RecordedFrame {
    RecordedFrame {
        pose: Pose {
            root: mat4_from_top3(&d.root),
            bones: d.bones.iter().map(mat4_from_top3).collect(),
            // Skeleton names live once on `Host::names`, not per frame.
            names: Vec::new(),
            camera: d
                .camera
                .map(|(pos, basis, fov)| (Vec3::from_array(pos), Mat3::from_cols_array_2d(&basis), fov)),
            velocity: Vec3::ZERO,
            tick: 0,
            state: String::new(),
        },
        time_secs: d.time,
    }
}

fn skate_demo_dir() -> PathBuf {
    PathBuf::from("iw4l-artifacts").join("skate-demos")
}

fn sanitize_demo_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(64)
        .collect();
    if cleaned.is_empty() { "demo".into() } else { cleaned }
}

fn free_pose_data(p: FreePose) -> ([f32; 3], f32, f32, f32) {
    (p.pos.to_array(), p.yaw, p.pitch, p.fov)
}

fn free_pose_from_data((pos, yaw, pitch, fov): ([f32; 3], f32, f32, f32)) -> FreePose {
    FreePose { pos: Vec3::from_array(pos), yaw, pitch, fov }
}

/// Snapshots the clip for saving. Cheap next to encoding it, so it runs on
/// the game thread and the JSON + gzip + disk write go to a worker.
fn demo_file(host: &Host, trim: (f32, f32)) -> Result<DemoFile, String> {
    let Some(first) = host.recorded.first() else {
        return Err("nothing recorded".into());
    };
    Ok(DemoFile {
        duration: host.recorded.last().map(|f| f.time_secs).unwrap_or(0.0),
        // The skeleton lives once per demo: prefer the shared list, fall
        // back to the first frame's copy for older recordings.
        names: if host.names.is_empty() {
            first.pose.names.clone()
        } else {
            host.names.clone()
        },
        frames: host.recorded.iter().map(frame_to_data).collect(),
        cuts: host
            .cuts
            .iter()
            .map(|c| CutData {
                time: c.time,
                cam: c.cam,
                free: c.free.map(free_pose_data),
            })
            .collect(),
        keyframes: host
            .keyframes
            .iter()
            .map(|k| KeyframeData {
                time: k.time,
                pos: k.pos.to_array(),
                yaw: k.yaw,
                pitch: k.pitch,
                fov: k.fov,
            })
            .collect(),
        ramps: host
            .speed_ramps
            .iter()
            .map(|r| RampData { time: r.time, speed: r.speed })
            .collect(),
        ease: !host.keyframe_linear,
        trim_start: trim.0,
        trim_end: trim.1,
        free_base: host.free_base.map(free_pose_data),
    })
}

/// Encodes and writes a clip. Several seconds of skating is tens of MB of
/// JSON, so this runs off the game thread to keep saving from freezing it.
fn write_demo(file: &DemoFile, name: &str) -> Result<PathBuf, String> {
    let dir = skate_demo_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}.iw4lskate", sanitize_demo_name(name)));
    let json = serde_json::to_vec(file).map_err(|e| e.to_string())?;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&json).map_err(|e| e.to_string())?;
    let compressed = encoder.finish().map_err(|e| e.to_string())?;
    // Write beside it and swap in, so a crash mid-save can't leave a
    // truncated clip where a good one was.
    let partial = path.with_extension("iw4lskate.partial");
    std::fs::write(&partial, compressed).map_err(|e| e.to_string())?;
    std::fs::rename(&partial, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

struct LoadedDemo {
    frames: Vec<RecordedFrame>,
    names: Vec<String>,
    cuts: Vec<CameraCut>,
    keyframes: Vec<Keyframe>,
    ramps: Vec<SpeedRamp>,
    ease: bool,
    trim: (f32, f32),
    free_base: Option<FreePose>,
}

fn load_demo(name: &str) -> Result<LoadedDemo, String> {
    let path = skate_demo_dir().join(format!("{}.iw4lskate", sanitize_demo_name(name)));
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
    let mut json = Vec::new();
    decoder.read_to_end(&mut json).map_err(|e| e.to_string())?;
    let file: DemoFile = serde_json::from_slice(&json).map_err(|e| e.to_string())?;
    Ok(LoadedDemo {
        frames: file.frames.into_iter().map(data_to_frame).collect(),
        names: file.names,
        cuts: file
            .cuts
            .into_iter()
            .map(|c| CameraCut {
                time: c.time,
                cam: c.cam,
                free: c.free.map(free_pose_from_data),
            })
            .collect(),
        keyframes: file
            .keyframes
            .into_iter()
            .map(|k| Keyframe {
                time: k.time,
                pos: Vec3::from_array(k.pos),
                yaw: k.yaw,
                pitch: k.pitch,
                fov: k.fov,
            })
            .collect(),
        ramps: file
            .ramps
            .into_iter()
            .map(|r| SpeedRamp { time: r.time, speed: r.speed })
            .collect(),
        ease: file.ease,
        trim: (file.trim_start, file.trim_end),
        free_base: file.free_base.map(free_pose_from_data),
    })
}

/// Reads a saved clip's length from the start of the file (newer saves put
/// it first), without decoding the frames.
fn peek_duration(path: &std::path::Path) -> Option<f32> {
    let bytes = std::fs::File::open(path).ok()?;
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut head = vec![0u8; 256];
    let mut filled = 0;
    while filled < head.len() {
        match decoder.read(&mut head[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => filled += n,
        }
    }
    let text = std::str::from_utf8(&head[..filled]).ok()?;
    let rest = text.split_once("\"duration\":")?.1;
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == 'e' || c == 'E' || c == '+'))
        .unwrap_or(rest.len());
    rest[..end].parse::<f32>().ok().filter(|d| *d > 0.0)
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
fn ago(modified: std::time::SystemTime) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        86_400..172_800 => "yesterday".into(),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// Saved replays, newest first: (name, "12.4 s · 5 min ago").
fn list_demos() -> Vec<(String, String)> {
    let Ok(dir) = std::fs::read_dir(skate_demo_dir()) else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, String, String)> = dir
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("iw4lskate") {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_string();
            let meta = entry.metadata().ok()?;
            let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            let size = match peek_duration(&path) {
                Some(d) => format!("{d:.1} s"),
                None => format!("{:.1} MB", meta.len() as f64 / 1_048_576.0),
            };
            Some((modified, name, format!("{size}  ·  {}", ago(modified))))
        })
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, name, info)| (name, info)).collect()
}

/// The next free "clip-001" style save name.
fn next_clip_name() -> String {
    let highest = std::fs::read_dir(skate_demo_dir())
        .map(|dir| {
            dir.filter_map(Result::ok)
                .filter_map(|e| {
                    let name = e.path().file_stem()?.to_str()?.to_string();
                    name.strip_prefix("clip-")?.parse::<u32>().ok()
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    format!("clip-{:03}", highest + 1)
}

const REPLAY_CAM_COUNT: u8 = 14;

fn replay_cam_name(cam: u8) -> &'static str {
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

fn replay_heading(mode: &SkateMode) -> Vec3 {
    let Some((eye, _)) = mode.camera.as_ref() else {
        return Vec3::X;
    };
    let f = eye.forward();
    let h = Vec3::new(f.x, f.y, 0.0);
    if h.length_squared() > 1.0e-6 { h.normalize() } else { Vec3::X }
}

fn free_cam_forward(yaw: f32, pitch: f32) -> Vec3 {
    Vec3::new(pitch.cos() * yaw.cos(), pitch.cos() * yaw.sin(), pitch.sin())
}

fn yaw_pitch_from_forward(f: Vec3) -> (f32, f32) {
    let f = if f.length_squared() > 1.0e-6 { f.normalize() } else { Vec3::X };
    (f.y.atan2(f.x), f.z.asin())
}

fn angle_diff(to: f32, from: f32) -> f32 {
    let mut d = (to - from).rem_euclid(std::f32::consts::TAU);
    if d > std::f32::consts::PI {
        d -= std::f32::consts::TAU;
    }
    d
}

fn catmull_rom(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    0.5 * ((2.0 * p1)
        + (-p0 + p2) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t * t
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t * t * t)
}

/// Smoothstep ease: zero velocity at both ends of the segment, which is what
/// makes keyframe moves settle instead of knocking at every key.
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Centripetal Catmull-Rom (alpha = 0.5). Uniform knot spacing makes the
/// camera surge when keyframes are unevenly placed; parameterizing by root
/// distance keeps the speed constant along the path.
fn catmull_rom_knots(
    k0: f32,
    k1: f32,
    k2: f32,
    k3: f32,
    t: f32,
    p0: Vec3,
    p1: Vec3,
    p2: Vec3,
    p3: Vec3,
) -> Vec3 {
    let d01 = k1 - k0;
    let d12 = k2 - k1;
    let d23 = k3 - k2;
    let d02 = k2 - k0;
    let d13 = k3 - k1;
    // Degenerate spans (duplicate keyframes): fall back to a segment lerp.
    if d01 <= 1.0e-6 || d12 <= 1.0e-6 || d02 <= 1.0e-6 || d13 <= 1.0e-6 {
        let e = ((t - k1) / d12.max(1.0e-6)).clamp(0.0, 1.0);
        return p1.lerp(p2, e);
    }
    let a1 = p0 * ((k1 - t) / d01) + p1 * ((t - k0) / d01);
    let a2 = p1 * ((k2 - t) / d12) + p2 * ((t - k1) / d12);
    let a3 = p2 * ((k3 - t) / d23) + p3 * ((t - k2) / d23);
    let b1 = a1 * ((k2 - t) / d02) + a2 * ((t - k0) / d02);
    let b2 = a2 * ((k3 - t) / d13) + a3 * ((t - k1) / d13);
    b1 * ((k2 - t) / d12) + b2 * ((t - k1) / d12)
}

/// The camera cut in force at `t`: the last cut at or before it.
fn active_cut(cuts: &[CameraCut], t: f32) -> Option<u8> {
    cuts.iter().rev().find(|c| c.time <= t).map(|c| c.cam)
}

/// Seconds a destructive control must be held before it fires.
const HOLD_DELETE: f32 = 0.5;
const HOLD_CONFIRM: f32 = 0.6;
const HOLD_EXIT: f32 = 0.8;
/// Replay seconds per real second at full left-stick scrub.
const SCRUB_MAX: f32 = 4.0;

/// A control that fires once after being held for a while.
#[derive(Default)]
struct Hold {
    secs: f32,
    fired: bool,
}

impl Hold {
    /// Advances while `down`; true on the frame the hold completes.
    fn tick(&mut self, down: bool, dt: f32, needed: f32) -> bool {
        if !down {
            self.secs = 0.0;
            self.fired = false;
            return false;
        }
        self.secs += dt;
        if !self.fired && self.secs >= needed {
            self.fired = true;
            return true;
        }
        false
    }

    /// Progress to show, once the hold is clearly deliberate rather than a tap.
    fn progress(&self, needed: f32) -> Option<f32> {
        (self.secs > 0.12 && !self.fired).then(|| (self.secs / needed).min(1.0))
    }
}

/// Turns a held direction into a first step plus an auto-repeat.
#[derive(Default)]
struct Repeat {
    dir: i8,
    timer: f32,
}

impl Repeat {
    fn tick(&mut self, dir: i8, dt: f32) -> i8 {
        if dir == 0 {
            self.dir = 0;
            self.timer = 0.0;
            return 0;
        }
        if dir != self.dir {
            self.dir = dir;
            self.timer = 0.35;
            return dir;
        }
        self.timer -= dt;
        if self.timer <= 0.0 {
            self.timer = 0.07;
            return dir;
        }
        0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Marker {
    Cut(usize),
    Keyframe(usize),
    Ramp(usize),
}

/// Timeline marker times for jumping and scrub snapping.
fn marker_times(host: &Host, mode: &SkateMode, end: f32) -> Vec<f32> {
    let mut times: Vec<f32> = host.keyframes.iter().map(|k| k.time).collect();
    times.extend(host.cuts.iter().map(|c| c.time));
    times.extend(host.speed_ramps.iter().map(|r| r.time));
    times.extend([mode.trim_start, mode.trim_end].into_iter().filter(|t| *t > 0.0));
    times.extend([0.0, end]);
    times
}

/// The marker nearest the playhead within a quarter second, if any.
fn delete_target(host: &Host, t: f32) -> Option<Marker> {
    let near = |x: f32| ((x - t).abs() < 0.25).then_some((x - t).abs());
    let mut best: Option<(f32, Marker)> = None;
    let mut consider = |d: Option<f32>, m: Marker| {
        if let Some(d) = d
            && best.is_none_or(|(b, _)| d < b)
        {
            best = Some((d, m));
        }
    };
    for (i, c) in host.cuts.iter().enumerate() {
        consider(near(c.time), Marker::Cut(i));
    }
    for (i, k) in host.keyframes.iter().enumerate() {
        consider(near(k.time), Marker::Keyframe(i));
    }
    for (i, r) in host.speed_ramps.iter().enumerate() {
        consider(near(r.time), Marker::Ramp(i));
    }
    best.map(|(_, m)| m)
}

/// What the marker is, for the delete prompt: "cut (Tripod)", "keyframe".
fn marker_describe(host: &Host, m: Marker) -> String {
    match m {
        Marker::Cut(i) => host
            .cuts
            .get(i)
            .map(|c| format!("cut ({})", cam_label(c.cam)))
            .unwrap_or_else(|| "cut".into()),
        Marker::Keyframe(_) => "keyframe".into(),
        Marker::Ramp(i) => host
            .speed_ramps
            .get(i)
            .map(|r| format!("speed ramp ({}x)", r.speed))
            .unwrap_or_else(|| "speed ramp".into()),
    }
}

fn marker_time(host: &Host, m: Marker) -> f32 {
    match m {
        Marker::Cut(i) => host.cuts.get(i).map(|c| c.time),
        Marker::Keyframe(i) => host.keyframes.get(i).map(|k| k.time),
        Marker::Ramp(i) => host.speed_ramps.get(i).map(|r| r.time),
    }
    .unwrap_or(0.0)
}

/// Moves a marker to `t`, keeps its list in time order and returns where it
/// ended up.
fn move_marker(host: &mut Host, m: Marker, t: f32) -> Marker {
    match m {
        Marker::Cut(i) if i < host.cuts.len() => {
            host.cuts[i].time = t;
            let moved = host.cuts[i].clone();
            host.cuts.sort_by(|a, b| a.time.total_cmp(&b.time));
            Marker::Cut(host.cuts.iter().position(|c| *c == moved).unwrap_or(i))
        }
        Marker::Keyframe(i) if i < host.keyframes.len() => {
            host.keyframes[i].time = t;
            let moved = host.keyframes[i].clone();
            host.keyframes.sort_by(|a, b| a.time.total_cmp(&b.time));
            Marker::Keyframe(host.keyframes.iter().position(|k| *k == moved).unwrap_or(i))
        }
        Marker::Ramp(i) if i < host.speed_ramps.len() => {
            host.speed_ramps[i].time = t;
            let moved = host.speed_ramps[i].clone();
            host.speed_ramps.sort_by(|a, b| a.time.total_cmp(&b.time));
            Marker::Ramp(host.speed_ramps.iter().position(|r| *r == moved).unwrap_or(i))
        }
        other => other,
    }
}

fn marker_exists(host: &Host, m: Marker) -> bool {
    match m {
        Marker::Cut(i) => i < host.cuts.len(),
        Marker::Keyframe(i) => i < host.keyframes.len(),
        Marker::Ramp(i) => i < host.speed_ramps.len(),
    }
}

fn marker_name(m: Marker) -> &'static str {
    match m {
        Marker::Cut(_) => "cut",
        Marker::Keyframe(_) => "keyframe",
        Marker::Ramp(_) => "speed ramp",
    }
}

fn delete_marker(host: &mut Host, m: Marker) -> String {
    match m {
        Marker::Cut(i) => {
            let c = host.cuts.remove(i);
            format!("Deleted cut ({}) at {:.2}s", cam_label(c.cam), c.time)
        }
        Marker::Keyframe(i) => format!("Deleted keyframe at {:.2}s", host.keyframes.remove(i).time),
        Marker::Ramp(i) => format!("Deleted speed ramp at {:.2}s", host.speed_ramps.remove(i).time),
    }
}

/// Shows a short message over the replay timeline and logs it.
fn toast(mode: &mut SkateMode, text: impl Into<String>) {
    toast_for(mode, text, 2.0);
}

/// A toast that stays up for `secs`.
fn toast_for(mode: &mut SkateMode, text: impl Into<String>, secs: f32) {
    let text = text.into();
    diag::info!(World, "Skate replay: {text}");
    mode.toast = Some((text, secs));
}

/// Camera name as shown on screen.
fn cam_label(cam: u8) -> &'static str {
    frame::replay_cam_label(cam)
}

/// Adds a keyframe from the view on screen, whichever camera produced it.
fn capture_keyframe(host: &mut Host, mode: &mut SkateMode, time: f32) {
    let Some((eye, fov)) = host.last_view.or(mode.camera) else {
        toast(mode, "No camera view to capture yet");
        return;
    };
    let (yaw, pitch) = yaw_pitch_from_forward(*eye.forward());
    // On an existing keyframe, re-capture the view into it instead of
    // stacking a second one on the same moment.
    if let Some(k) = host.keyframes.iter_mut().find(|k| (k.time - time).abs() < 0.15) {
        k.pos = eye.translation;
        k.yaw = yaw;
        k.pitch = pitch;
        k.fov = fov;
        let at = k.time;
        toast(mode, format!("Keyframe at {at:.2}s updated to this view"));
        return;
    }
    host.keyframes.push(Keyframe { time, pos: eye.translation, yaw, pitch, fov });
    host.keyframes.sort_by(|a, b| a.time.total_cmp(&b.time));
    toast(mode, format!("Keyframe added at {time:.2}s"));
}

/// Hard cut: camera `cam` shows from `time` on. Replaces a cut already near
/// the playhead, so repeated presses don't pile cuts onto one frame.
fn cut_to(host: &mut Host, mode: &mut SkateMode, time: f32, cam: u8) {
    if let Some(i) = host.cuts.iter().position(|c| (c.time - time).abs() < 0.15) {
        host.cuts[i].cam = cam;
    } else {
        host.cuts.push(CameraCut { time, cam, free: None });
        host.cuts.sort_by(|a, b| a.time.total_cmp(&b.time));
    }
    toast(mode, format!("Cut to {} at {time:.2}s", cam_label(cam)));
}

/// X / `skate cut`: cut here with the camera on screen; tapped again, advances
/// that cut to the next camera, so one button walks through the shots.
fn add_cut(host: &mut Host, mode: &mut SkateMode, time: f32) {
    let current = active_cut(&host.cuts, time).unwrap_or(mode.replay_cam);
    if let Some(i) = host.cuts.iter().position(|c| (c.time - time).abs() < 0.15) {
        let next = (current + 1) % REPLAY_CAM_COUNT;
        host.cuts[i].cam = next;
        toast(mode, format!("Cut at {time:.2}s: {}", cam_label(next)));
    } else {
        cut_to(host, mode, time, current);
    }
}

/// Steps the camera under the playhead: the active cut's camera if a cut
/// covers it, otherwise the base replay camera. While the replay is playing,
/// switching the camera is itself a hard cut, so a new shot lands wherever
/// you press it. Returns true when a cut was stamped or re-aimed.
fn cycle_camera(host: &mut Host, mode: &mut SkateMode, dir: i8, playing: bool) -> bool {
    let t = host.replay_clock_secs;
    let step = |cam: u8| ((cam as i32 + dir as i32).rem_euclid(REPLAY_CAM_COUNT as i32)) as u8;
    // Re-aim the cut right under the playhead, e.g. the one just stamped.
    if let Some(i) = host.cuts.iter().rposition(|c| (c.time - t).abs() < 0.15) {
        host.cuts[i].cam = step(host.cuts[i].cam);
        let (time, cam) = (host.cuts[i].time, host.cuts[i].cam);
        toast(mode, format!("Cut at {time:.2}s: {}", cam_label(cam)));
        return true;
    }
    if playing {
        // Live switching: mid-playback the camera change *is* the cut.
        let cam = step(active_cut(&host.cuts, t).unwrap_or(mode.replay_cam));
        mode.replay_cam = cam;
        cut_to(host, mode, t, cam);
        return true;
    }
    if let Some(i) = host.cuts.iter().rposition(|c| c.time <= t) {
        host.cuts[i].cam = step(host.cuts[i].cam);
        let (time, cam) = (host.cuts[i].time, host.cuts[i].cam);
        toast(mode, format!("Cut at {time:.2}s: {}", cam_label(cam)));
        return true;
    }
    mode.replay_cam = step(mode.replay_cam);
    toast(mode, format!("Camera: {}", cam_label(mode.replay_cam)));
    false
}

/// The context prompts under the replay timeline: only the controls that do
/// something right now, for the device used last.
fn replay_prompts(mode: &SkateMode, layer: bool, wheel: bool) -> Vec<(String, String)> {
    let mut p: Vec<(String, String)> = Vec::new();
    let mut add = |key: &str, action: &str| p.push((key.to_string(), action.to_string()));
    let pad = mode.pad_prompts;
    let paused = mode.replay_paused;
    let cam = mode.eff_cam;
    let steer = frame::replay_cam_steerable(cam);
    let cam_ctrl = steer && mode.cam_control;
    let free = cam == 6;

    if mode.menu_open && mode.load_open {
        if pad {
            add("D-pad", "Choose");
            add("A", "Load");
            add("B", "Back");
        } else {
            add("Up/Down", "Choose");
            add("Enter", "Load");
            add("Tab", "Close");
        }
        return p;
    }
    if mode.menu_open {
        let item = REPLAY_MENU[mode.menu_focus.min(REPLAY_MENU.len() - 1)];
        if pad {
            add("D-pad", "Move");
            match item.kind() {
                ReplayMenuKind::Adjust => add("D-pad L/R", "Change"),
                ReplayMenuKind::Action => add("A", "Select"),
                ReplayMenuKind::Hold => add("Hold A", "Confirm"),
            }
            add("LB/RB", "Section");
            add("B", "Close");
        } else {
            add("Up/Down", "Move");
            match item.kind() {
                ReplayMenuKind::Adjust => add("Left/Right", "Change"),
                ReplayMenuKind::Action => add("Enter", "Select"),
                ReplayMenuKind::Hold => add("Hold Enter", "Confirm"),
            }
            add("Tab", "Close");
        }
        return p;
    }

    if let Some((_, what)) = &mode.grabbed {
        if pad {
            add("LS", &format!("Move {what}"));
            add("D-pad L/R", "Nudge a frame");
            add("A", "Put down");
            add("B", "Put down");
        } else {
            add(", .", &format!("Move {what}"));
            add("G", "Put down");
        }
        return p;
    }

    if pad && wheel {
        let pick = mode.cam_wheel.unwrap_or(cam);
        add("RS", "Point at a camera");
        add("D-pad L/R", "Step");
        add("Release X", &format!("Cut to {}", frame::replay_cam_label(pick)));
        add("B", "Cancel");
        return p;
    }

    if pad && layer {
        add("D-pad L/R", "Trim in / out");
        add("D-pad Up", "Speed ramp");
        add("D-pad Down", "Clear trim");
        add("LS", "Zoom / roll");
        add("X", "9:16 frame");
        add("A", "Cinematic bars");
        add("B", "Undo");
        add("Y", "Redo");
        return p;
    }

    if pad {
        add("A", if paused { "Play" } else { "Pause" });
        if cam_ctrl {
            if free {
                add("LS", "Fly");
                add("RS", "Look");
                add("LT/RT", "Down / up");
            } else {
                add("RS", "Aim");
                add("LT/RT", "Dolly");
            }
            add("RS click", "Playback controls");
        } else {
            if paused {
                add("LS", "Scrub");
            } else {
                add("LT/RT", "Slow / fast");
            }
            if steer {
                add("RS click", "Move camera");
            }
        }
        if let Some((_, what)) = &mode.delete_target {
            add("B", &format!("Move {what}"));
            add("Hold B", "Delete it");
        } else if paused {
            add("D-pad", "Frame / marker");
        }
        if !paused {
            add("LB/RB", "Switch camera");
        }
        add("X", "Cut (hold: pick)");
        if paused {
            add("Y", "Keyframe");
        }
        add("Hold RB", "Quick edit");
        add("Back", "All controls");
    } else {
        add("Space", if paused { "Play" } else { "Pause" });
        if free {
            add("WASD", "Fly");
            add("Arrows", "Look");
        } else if steer {
            add("Arrows", "Aim");
        } else if paused {
            add(", .", "Frame step");
        }
        if let Some((_, what)) = &mode.delete_target {
            add("G", &format!("Move {what}"));
            add("Hold Bksp", "Delete it");
        }
        add("X / 1-0", "Cut");
        add("K", "Keyframe");
        add("Ctrl+Z", "Undo");
        add("Tab", "Menu");
        add("/", "All controls");
    }
    p
}

/// Render resolutions; the empty entry keeps the window's own size.
const RES_PRESETS: [&str; 5] = ["", "1280x720", "1920x1080", "2560x1440", "3840x2160"];
/// Classic cinemascope: the visible band of the letterbox template.
const CINEMATIC_ASPECT: f32 = 2.39;

fn menu_value(item: ReplayMenuItem, mode: &SkateMode, host: &Host) -> String {
    let at = |t: f32, none: &str| if t > 0.0 { format!("{t:.2}s") } else { none.to_string() };
    let steps = |n: usize| match n {
        0 => "none".to_string(),
        1 => "1 step".to_string(),
        n => format!("{n} steps"),
    };
    match item {
        ReplayMenuItem::TrimIn => at(mode.trim_start, "start"),
        ReplayMenuItem::TrimOut => at(mode.trim_end, "end"),
        ReplayMenuItem::Speed => if host.speed_ramps.is_empty() {
            format!("{}x", mode.replay_speed)
        } else {
            format!("next ramp {}x", mode.replay_speed)
        },
        ReplayMenuItem::Load => String::new(),
        ReplayMenuItem::RampAdd => format!("{}x at {:.2}s", mode.replay_speed, host.replay_clock_secs),
        ReplayMenuItem::RampsClear => host.speed_ramps.len().to_string(),
        ReplayMenuItem::KeyframesClear => host.keyframes.len().to_string(),
        ReplayMenuItem::CutsClear => host.cuts.len().to_string(),
        ReplayMenuItem::Undo => steps(host.undo.len()),
        ReplayMenuItem::Redo => steps(host.redo.len()),
        ReplayMenuItem::Fov => format!("{:.0}", cam_fov(mode)),
        ReplayMenuItem::Roll => format!("{:+.0}", mode.replay_roll.to_degrees()),
        ReplayMenuItem::Easing => if host.keyframe_linear { "Linear" } else { "Smooth" }.into(),
        ReplayMenuItem::Skin => assets::bot_model::skin_names()
            .get(assets::bot_model::selected_skin())
            .cloned()
            .unwrap_or_else(|| "Default".into()),
        ReplayMenuItem::Fps => format!("{:.0}", mode.render_fps),
        ReplayMenuItem::Blur => match mode.render_shutter {
            0 | 1 => "Off".into(),
            n => format!("{n}x"),
        },
        ReplayMenuItem::Resolution => if mode.portrait {
            "1080x1920 (TikTok)".into()
        } else if mode.render_scale.is_empty() {
            "Native".into()
        } else {
            mode.render_scale.clone()
        },
        ReplayMenuItem::Quality => format!("CRF {}", mode.render_crf),
        ReplayMenuItem::Portrait => if mode.portrait { "On" } else { "Off" }.into(),
        ReplayMenuItem::Cinematic => if mode.cinematic { "On" } else { "Off" }.into(),
        ReplayMenuItem::TrimClear | ReplayMenuItem::Save | ReplayMenuItem::Render => String::new(),
    }
}

fn menu_adjust(item: ReplayMenuItem, dir: i8, mode: &mut SkateMode, host: &mut Host) {
    let d = dir as f32;
    match item {
        ReplayMenuItem::Speed => {
            mode.replay_speed = if dir > 0 {
                (mode.replay_speed * 2.0).min(4.0)
            } else {
                (mode.replay_speed * 0.5).max(0.125)
            };
        }
        ReplayMenuItem::Fov => mode.replay_fov = (cam_fov(mode) + 5.0 * d).clamp(10.0, 120.0),
        ReplayMenuItem::Roll => mode.replay_roll = (mode.replay_roll + 0.05 * d).clamp(-0.6, 0.6),
        ReplayMenuItem::Easing => host.keyframe_linear = !host.keyframe_linear,
        ReplayMenuItem::Skin => assets::bot_model::cycle_skin(dir as i32),
        ReplayMenuItem::Fps => mode.render_fps = (mode.render_fps + 15.0 * d).clamp(15.0, 240.0),
        ReplayMenuItem::Blur => {
            mode.render_shutter = (mode.render_shutter as i32 + dir as i32).clamp(1, 8) as u32;
        }
        ReplayMenuItem::Resolution => {
            let n = RES_PRESETS.len() as i32;
            let i = RES_PRESETS.iter().position(|p| *p == mode.render_scale).unwrap_or(0) as i32;
            mode.render_scale = RES_PRESETS[(i + dir as i32).rem_euclid(n) as usize].to_string();
        }
        // Higher quality is a lower CRF.
        ReplayMenuItem::Quality => {
            mode.render_crf = (mode.render_crf as i32 - dir as i32).clamp(12, 28) as u32;
        }
        ReplayMenuItem::Portrait => {
            mode.portrait = !mode.portrait;
            if mode.portrait {
                mode.cinematic = false;
            }
        }
        ReplayMenuItem::Cinematic => {
            mode.cinematic = !mode.cinematic;
            if mode.cinematic {
                mode.portrait = false;
            }
        }
        _ => {}
    }
}

fn menu_action(item: ReplayMenuItem, mode: &mut SkateMode, host: &mut Host) {
    let t = host.replay_clock_secs;
    match item {
        ReplayMenuItem::TrimIn => {
            mode.trim_start = t;
            if mode.trim_end > 0.0 && mode.trim_end <= t {
                mode.trim_end = 0.0;
            }
            toast(mode, format!("Trim in: {t:.2}s"));
        }
        ReplayMenuItem::TrimOut => {
            mode.trim_end = t;
            if mode.trim_start >= t {
                mode.trim_start = 0.0;
            }
            toast(mode, format!("Trim out: {t:.2}s"));
        }
        ReplayMenuItem::TrimClear => {
            mode.trim_start = 0.0;
            mode.trim_end = 0.0;
            toast(mode, "Trim cleared");
        }
        ReplayMenuItem::RampAdd => {
            host.speed_ramps.push(SpeedRamp { time: t, speed: mode.replay_speed });
            host.speed_ramps.sort_by(|a, b| a.time.total_cmp(&b.time));
            toast(mode, format!("Speed ramp: {}x at {t:.2}s", mode.replay_speed));
        }
        ReplayMenuItem::RampsClear => {
            host.speed_ramps.clear();
            toast(mode, "Speed ramps cleared");
        }
        ReplayMenuItem::KeyframesClear => {
            host.keyframes.clear();
            toast(mode, "Keyframes cleared");
        }
        ReplayMenuItem::CutsClear => {
            host.cuts.clear();
            toast(mode, "Cuts cleared");
        }
        ReplayMenuItem::Undo => undo_edit(host, mode),
        ReplayMenuItem::Redo => redo_edit(host, mode),
        ReplayMenuItem::Save => {
            mode.save_requested = Some(next_clip_name());
        }
        ReplayMenuItem::Load => {
            mode.load_entries = list_demos();
            if mode.load_entries.is_empty() {
                toast(mode, "No saved replays yet. Save one first");
            } else {
                mode.load_open = true;
                mode.load_focus = 0;
            }
        }
        ReplayMenuItem::Render => {
            mode.menu_open = false;
            mode.render_requested = true;
        }
        _ => {}
    }
}

fn cam_fov(mode: &SkateMode) -> f32 {
    if mode.replay_fov > 1.0 { mode.replay_fov } else { 55.0 }
}

/// Rolls a camera transform around its own forward axis (Dutch angle).
fn apply_roll(mut t: Transform, roll: f32) -> Transform {
    if roll.abs() > 1.0e-6 {
        t.rotation = Quat::from_axis_angle(*t.forward(), roll) * t.rotation;
    }
    t
}

/// Playback speed at clip time `t`. Without ramps the whole clip plays at
/// `base`. With ramps it plays at normal speed until the first one, and each
/// ramp eases into its speed over `RAMP_BLEND` seconds and holds it until the
/// next. The preview and the rendered video both use this, so they match.
fn clip_speed(ramps: &[SpeedRamp], base: f32, t: f32) -> f32 {
    if ramps.is_empty() {
        return base;
    }
    let mut speed = 1.0;
    for r in ramps {
        if t < r.time {
            break;
        }
        let u = smoothstep((t - r.time) / RAMP_BLEND);
        speed += (r.speed - speed) * u;
    }
    speed.max(0.01)
}

/// Starts the free cam from `view` (the camera on screen when switching in),
/// so it picks up exactly where the previous camera was looking.
fn init_free_cam(mode: &mut SkateMode, view: Option<(Transform, f32)>) {
    if let Some((eye, fov)) = view.or(mode.camera) {
        mode.free_cam_pos = eye.translation;
        let (yaw, pitch) = yaw_pitch_from_forward(*eye.forward());
        mode.free_cam_yaw = yaw;
        mode.free_cam_pitch = pitch;
        mode.free_cam_fov = fov;
    }
}

/// `pad_move`: the left stick and triggers fly the camera (camera control
/// on). With it off they stay on playback and only the right stick looks.
fn update_free_cam(
    mode: &mut SkateMode,
    keys: &ButtonInput<KeyCode>,
    input: &InputFrame,
    dt: f32,
    pad_move: bool,
) -> Vec3 {
    let move_speed = 300.0 * dt;
    let rot_speed = 1.6 * dt;
    let forward = free_cam_forward(mode.free_cam_yaw, 0.0);
    let right = Vec3::new(forward.y, -forward.x, 0.0);
    let mut delta = Vec3::ZERO;
    if keys.pressed(KeyCode::KeyW) { delta += forward; }
    if keys.pressed(KeyCode::KeyS) { delta -= forward; }
    if keys.pressed(KeyCode::KeyD) { delta += right; }
    if keys.pressed(KeyCode::KeyA) { delta -= right; }
    if keys.pressed(KeyCode::KeyE) { delta += Vec3::Z; }
    if keys.pressed(KeyCode::KeyQ) { delta -= Vec3::Z; }

    let left = if pad_move { stick_curve(input.left_stick()) } else { [0.0; 2] };
    let rstick = stick_curve(input.right_stick());
    let trig = if pad_move { input.triggers() } else { [0.0; 2] };
    // XInput reports thumbstick Y positive when pushed up, so stick-up flies
    // the way the camera looks and stick-down backs up.
    delta += forward * left[1];
    delta += right * left[0];
    // Worn triggers rest slightly pressed; gate them behind a deadzone.
    let up = (trig[1] - trig[0]).clamp(-1.0, 1.0);
    let up = if up.abs() < 0.1 { 0.0 } else { up };
    delta += Vec3::Z * up;
    // Move at the stick's magnitude. A constant speed turns any input above
    // the deadzone into a full-speed run, and a stick resting on the deadzone
    // edge then flaps the camera forward and back with no way to stop it.
    let mag = delta.length();
    if mag > 1.0e-6 {
        mode.free_cam_pos += delta.normalize() * (move_speed * mag.min(1.0));
    }

    let mut yaw_d = 0.0;
    let mut pitch_d = 0.0;
    if keys.pressed(KeyCode::ArrowLeft) { yaw_d += 1.0; }
    if keys.pressed(KeyCode::ArrowRight) { yaw_d -= 1.0; }
    if keys.pressed(KeyCode::ArrowUp) { pitch_d += 1.0; }
    if keys.pressed(KeyCode::ArrowDown) { pitch_d -= 1.0; }
    yaw_d -= rstick[0];
    pitch_d += rstick[1];
    mode.free_cam_yaw += yaw_d * rot_speed;
    mode.free_cam_pitch = (mode.free_cam_pitch + pitch_d * rot_speed).clamp(-1.5, 1.5);

    if keys.pressed(KeyCode::Minus) { mode.free_cam_fov = (mode.free_cam_fov - 30.0 * dt).clamp(10.0, 120.0); }
    if keys.pressed(KeyCode::Equal) { mode.free_cam_fov = (mode.free_cam_fov + 30.0 * dt).clamp(10.0, 120.0); }
    delta
}

fn keyframe_camera(mode: &mut SkateMode, clock_secs: f32, keyframes: &[Keyframe], ease: bool) {
    if keyframes.is_empty() {
        return;
    }
    let n = keyframes.len();
    let mut before = 0usize;
    for (i, kf) in keyframes.iter().enumerate() {
        if kf.time <= clock_secs {
            before = i;
        } else {
            break;
        }
    }
    let after = (before + 1).min(n - 1);
    let (pos, yaw, pitch, fov) = if before == after {
        let kf = &keyframes[before];
        (kf.pos, kf.yaw, kf.pitch, kf.fov)
    } else {
        let i0 = before.saturating_sub(1);
        let i1 = before;
        let i2 = after;
        let i3 = (after + 1).min(n - 1);
        let p0 = &keyframes[i0];
        let p1 = &keyframes[i1];
        let p2 = &keyframes[i2];
        let p3 = &keyframes[i3];
        let span = (p2.time - p1.time).max(1.0e-6);
        let u = ((clock_secs - p1.time) / span).clamp(0.0, 1.0);

        let y0 = p0.yaw;
        let y1 = y0 + angle_diff(p1.yaw, y0);
        let y2 = y1 + angle_diff(p2.yaw, y1);
        let y3 = y2 + angle_diff(p3.yaw, y2);
        let q0 = p0.pitch;
        let q1 = q0 + angle_diff(p1.pitch, q0);
        let q2 = q1 + angle_diff(p2.pitch, q1);
        let q3 = q2 + angle_diff(p3.pitch, q2);

        let linear = (
            p1.pos.lerp(p2.pos, u),
            p1.yaw + angle_diff(p2.yaw, p1.yaw) * u,
            p1.pitch + angle_diff(p2.pitch, p1.pitch) * u,
            p1.fov + (p2.fov - p1.fov) * u,
        );

        if ease {
            // Eased spline: smoothstep on the segment parameter plus
            // centripetal knots, so uneven keyframe spacing can't cause
            // velocity surges or corner hits.
            let e = smoothstep(u);
            let alpha = 0.5f32;
            let d = |a: Vec3, b: Vec3| (a - b).length().powf(alpha);
            let t0 = 0.0f32;
            let t1 = t0 + d(p1.pos, p0.pos);
            let t2 = t1 + d(p2.pos, p1.pos);
            let t3 = t2 + d(p3.pos, p2.pos);
            let tt = t1 + (t2 - t1) * e;
            let eased = (
                catmull_rom_knots(t0, t1, t2, t3, tt, p0.pos, p1.pos, p2.pos, p3.pos),
                catmull_rom(y0, y1, y2, y3, e),
                catmull_rom(q0, q1, q2, q3, e),
                catmull_rom(p0.fov, p1.fov, p2.fov, p3.fov, e),
            );
            // A duplicate or non-finite keyframe must not smear NaN into the
            // camera; fall back to the straight segment.
            let (pos, yaw, pitch, fov) = eased;
            if pos.is_finite() && yaw.is_finite() && pitch.is_finite() && fov.is_finite() {
                eased
            } else {
                linear
            }
        } else {
            // Straight lerp for deliberately hard, mechanical moves.
            linear
        }
    };
    let forward = free_cam_forward(yaw, pitch);
    mode.camera = Some((Transform::from_translation(pos).looking_to(forward, Vec3::Z), fov));
}

/// How far behind its ideal spot the follow camera may fall before it cuts
/// straight there (world inches; its ideal offset is about 250).
const FOLLOW_SNAP_DIST: f32 = 800.0;

fn apply_replay_camera(
    mode: &mut SkateMode,
    clock_secs: f32,
    heading: Vec3,
    host: &mut Host,
    dt: f32,
) {
    // A cut at or before the playhead overrides the cycled camera mode.
    let cam = active_cut(&host.cuts, clock_secs).unwrap_or(mode.replay_cam);
    match cam {
        0 => return,
        6 => {
            let forward = free_cam_forward(mode.free_cam_yaw, mode.free_cam_pitch);
            mode.camera = Some((
                apply_roll(
                    Transform::from_translation(mode.free_cam_pos).looking_to(forward, Vec3::Z),
                    mode.replay_roll,
                ),
                mode.free_cam_fov,
            ));
            return;
        }
        7 => {
            keyframe_camera(mode, clock_secs, &host.keyframes, !host.keyframe_linear);
            if let Some((t, fov)) = mode.camera.take() {
                mode.camera = Some((apply_roll(t, mode.replay_roll), fov));
            }
            return;
        }
        8 => {
            first_person_camera(mode);
            return;
        }
        9 => {
            // Fisheye: low and close with a very wide lens, aimed slightly up
            // the path ahead of the skater.
            let subject = mode.root.w_axis.truncate();
            let eye = subject - heading * 110.0 + Vec3::Z * 42.0;
            let fov = if mode.replay_fov > 1.0 { mode.replay_fov } else { 140.0 };
            let target = subject + heading * 40.0 + Vec3::Z * 30.0;
            let delta = target - eye;
            let dir = if delta.length_squared() > 1.0e-6 { delta.normalize() } else { -Vec3::Z };
            mode.camera = Some((
                apply_roll(Transform::from_translation(eye).looking_to(dir, Vec3::Z), mode.replay_roll),
                fov,
            ));
            return;
        }
        10 => {
            // Tripod: a fixed world position that pans and tilts to keep the
            // skater framed. Each tripod cut stands where the camera was when
            // it was cut to (`enter_tripod_slot`), so scrub to a spot, then
            // cut to tripod. Only without any view to take does it latch here.
            if !host.tripod_init {
                if let Some((eye, _)) = mode.camera.as_ref() {
                    host.tripod_pos = eye.translation;
                }
                host.tripod_init = true;
            }
            let subject = mode.root.w_axis.truncate();
            let eye = host.tripod_pos;
            let delta = subject - eye;
            let dir = if delta.length_squared() > 1.0e-6 { delta.normalize() } else { -Vec3::Z };
            mode.camera = Some((
                apply_roll(Transform::from_translation(eye).looking_to(dir, Vec3::Z), mode.replay_roll),
                cam_fov(mode),
            ));
            return;
        }
        11 => {
            // Follow: a chase camera that eases toward its ideal offset behind
            // the skater instead of snapping, so cuts into it glide rather
            // than jump.
            let subject = mode.root.w_axis.truncate();
            let ideal = subject - heading * 240.0 + Vec3::Z * 60.0;
            // A scrub, marker jump or replay restart moves the skater
            // somewhere else entirely: cut there instead of gliding across
            // the map to catch up.
            if !host.follow_init || host.follow_eye.distance(ideal) > FOLLOW_SNAP_DIST {
                host.follow_eye = ideal;
                host.follow_init = true;
            }
            let k = 1.0 - (-6.0 * dt.max(1.0e-6)).exp();
            host.follow_eye = host.follow_eye.lerp(ideal, k);
            let eye = host.follow_eye;
            let delta = subject - eye;
            let dir = if delta.length_squared() > 1.0e-6 { delta.normalize() } else { -Vec3::Z };
            mode.camera = Some((
                apply_roll(Transform::from_translation(eye).looking_to(dir, Vec3::Z), mode.replay_roll),
                cam_fov(mode),
            ));
            return;
        }
        12 => {
            // Body cam: locked to the chest, free to aim. Spin with the
            // skater and steer around them.
            bone_camera(mode, &["SPINE3", "SPINE1", "HIPS"], 2.0, host.locked_yaw, host.locked_pitch, host.locked_dist);
            return;
        }
        13 => {
            // Board cam: locked to the deck itself, free to aim.
            bone_camera(mode, &["SKATEBOARD_ROOT"], 2.5, host.locked_yaw, host.locked_pitch, host.locked_dist);
            return;
        }
        _ => {}
    }
    let fov = cam_fov(mode);
    let subject = mode.root.w_axis.truncate();
    let right = Vec3::new(heading.y, -heading.x, 0.0);
    // The world is in inches; keep the camera a few feet off the skater.
    let (eye, up) = match cam {
        1 => {
            let a = clock_secs * 0.6;
            (subject + Vec3::new(a.cos(), a.sin(), 0.0) * 160.0 + Vec3::Z * 70.0, Vec3::Z)
        }
        2 => (subject - right * 150.0 + Vec3::Z * 60.0, Vec3::Z),
        3 => (subject + right * 150.0 + Vec3::Z * 60.0, Vec3::Z),
        4 => (subject + Vec3::Z * 260.0, Vec3::Y),
        _ => (subject - heading * 220.0 + Vec3::Z * 50.0, Vec3::Z),
    };
    let delta = subject - eye;
    let dir = if delta.length_squared() > 1.0e-6 { delta.normalize() } else { -Vec3::Z };
    mode.camera = Some((apply_roll(Transform::from_translation(eye).looking_to(dir, up), mode.replay_roll), fov));
}

/// First-person camera pinned to the skater's head bone, inheriting its
/// pitch/yaw. The skate rig uses +Y forward / +X up; after `convert` conjugates
/// through the skate->world basis those land on the world matrix's z_axis
/// (forward) and x_axis (up).
fn first_person_camera(mode: &mut SkateMode) {
    let fov = cam_fov(mode);
    if let Some(index) = mode.names.iter().position(|n| n == "HEAD")
        && let Some(bone) = mode.bones.get(index)
    {
        // Bones are in root-relative animation space; compose the world
        // root to place the head in the actual world.
        let world = mode.root * rig::convert(*bone);
        let forward = world.z_axis.truncate().normalize_or_zero();
        let mut up = world.x_axis.truncate().normalize_or_zero();
        if up.length_squared() < 1.0e-6 {
            up = Vec3::Z;
        }
        let eye = world.w_axis.truncate() + forward * 5.0;
        mode.camera = Some((
            apply_roll(Transform::from_translation(eye).looking_to(forward, up), mode.replay_roll),
            fov,
        ));
        return;
    }
    // Fallback: chase-style behind the root if the head bone is absent.
    let subject = mode.root.w_axis.truncate();
    let heading = replay_heading(mode);
    let eye = subject - heading * 220.0 + Vec3::Z * 50.0;
    let delta = subject - eye;
    let dir = if delta.length_squared() > 1.0e-6 { delta.normalize() } else { -Vec3::Z };
    mode.camera = Some((apply_roll(Transform::from_translation(eye).looking_to(dir, Vec3::Z), mode.replay_roll), fov));
}

/// Radial deadzone and rescale for a thumbstick, shared by the free cam and
/// the body-locked aim. XInput thumbsticks drift when they don't return to
/// exact centre; residual tilt must not keep steering the camera.
fn stick_curve(v: [f32; 2]) -> [f32; 2] {
    let dead = 0.35;
    let m = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if m < dead {
        [0.0, 0.0]
    } else {
        let s = (m - dead) / (1.0 - dead) / m;
        [v[0] * s, v[1] * s]
    }
}

/// Camera bolted to a rig bone: it inherits every bone rotation, so spins,
/// flips and board motion drag the view with them (action-cam style). `yaw`
/// and `pitch` aim the mount freely relative to the bone, and `dist` dollies
/// it back along the aim. Tries each bone name in turn, so a rig missing one
/// still finds a nearby mount.
fn bone_camera(mode: &mut SkateMode, bones: &[&str], up_offset: f32, yaw: f32, pitch: f32, dist: f32) {
    let fov = cam_fov(mode);
    for name in bones {
        let Some(index) = mode.names.iter().position(|n| n == *name) else {
            continue;
        };
        let Some(bone) = mode.bones.get(index) else {
            continue;
        };
        let world = mode.root * rig::convert(*bone);
        let forward = world.z_axis.truncate().normalize_or_zero();
        let up = world.x_axis.truncate().normalize_or_zero();
        if forward.length_squared() < 1.0e-6 || up.length_squared() < 1.0e-6 {
            continue;
        }
        let side = forward.cross(up).normalize_or_zero();
        if side.length_squared() < 1.0e-6 {
            continue;
        }
        let aim = Quat::from_axis_angle(up, yaw) * Quat::from_axis_angle(side, pitch);
        let look = aim * forward;
        let cam_up = aim * up;
        let eye = world.w_axis.truncate() - look * dist + cam_up * up_offset;
        mode.camera = Some((
            apply_roll(Transform::from_translation(eye).looking_to(look, cam_up), mode.replay_roll),
            fov,
        ));
        return;
    }
    // Fallback: chase-style behind the root if every named bone is absent.
    let subject = mode.root.w_axis.truncate();
    let heading = replay_heading(mode);
    let eye = subject - heading * 220.0 + Vec3::Z * 50.0;
    let delta = subject - eye;
    let dir = if delta.length_squared() > 1.0e-6 { delta.normalize() } else { -Vec3::Z };
    mode.camera = Some((apply_roll(Transform::from_translation(eye).looking_to(dir, Vec3::Z), mode.replay_roll), fov));
}

fn render_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    PathBuf::from("iw4l-artifacts")
        .join("render")
        .join(format!("{}-{stamp}", sanitize_demo_name(name)))
}

/// Everything ffmpeg needs, decided when the render starts. The pipe itself
/// spawns on the first captured frame, whose size and pixel order are only
/// known then.
struct RenderPipeConfig {
    fps: f32,
    shutter: u32,
    crf: u32,
    scale: String,
    portrait: bool,
    cinematic: bool,
    name: String,
    dir: PathBuf,
}

/// A render's live ffmpeg process. Frames stream into its stdin, so nothing
/// is cached on disk: a 4K render costs the output file and a few frames of
/// memory instead of tens of gigabytes of raw frames.
struct RenderPipe {
    sender: Option<mpsc::SyncSender<Vec<u8>>>,
    writer: Option<std::thread::JoinHandle<()>>,
    child: Option<std::process::Child>,
    out: PathBuf,
}

impl SkateRenderState {
    /// Sends one captured frame to ffmpeg, starting it on the first frame.
    fn feed_frame(&mut self, image: &bevy::image::Image) -> Result<(), String> {
        let Some(data) = image.data.as_ref() else {
            return Err("screenshot readback has no pixel data".into());
        };
        if self.pipe.is_none() {
            let cfg = self
                .pipe_cfg
                .as_ref()
                .ok_or("render pipeline not configured")?;
            self.pipe = Some(spawn_pipe(cfg, image.width(), image.height(), self.bgra)?);
        }
        let sender = self
            .pipe
            .as_ref()
            .and_then(|pipe| pipe.sender.as_ref())
            .ok_or("render pipe closed")?;
        // Hand ffmpeg the pixels exactly as captured: one memcpy beats
        // converting every pixel on the game thread, and ffmpeg converts
        // them itself on its own core.
        sender
            .send(data.clone())
            .map_err(|_| "ffmpeg stopped reading frames".to_string())
    }

    /// Closes the stream and waits for the encode on a worker, so the game
    /// doesn't stall on the encoder's final flush.
    fn finish_pipe(&mut self) {
        let Some(mut pipe) = self.pipe.take() else {
            return;
        };
        self.pipe_cfg = None;
        pipe.sender = None;
        let writer = pipe.writer.take();
        let child = pipe.child.take();
        let out = pipe.out.clone();
        let _ = std::thread::Builder::new()
            .name("skate-render-finish".into())
            .spawn(move || {
                if let Some(writer) = writer {
                    let _ = writer.join();
                }
                let Some(mut child) = child else {
                    return;
                };
                match child.wait() {
                    Ok(status) if status.success() => {
                        if let Some(dir) = out.parent() {
                            cleanup_frames(dir);
                        }
                        diag::info!(World, "Skate video ready: {}", out.display());
                    }
                    Ok(status) => {
                        let mut stderr = String::new();
                        if let Some(mut e) = child.stderr.take() {
                            let _ = e.read_to_string(&mut stderr);
                        }
                        diag::warn!(World, "Skate encode failed ({status}): {stderr}");
                    }
                    Err(e) => diag::warn!(World, "Skate encode error: {e}"),
                }
            });
    }

    /// Stops a render in progress: kills ffmpeg and removes the partial file.
    fn abort_pipe(&mut self) {
        let Some(mut pipe) = self.pipe.take() else {
            self.pipe_cfg = None;
            return;
        };
        self.pipe_cfg = None;
        pipe.sender = None;
        if let Some(mut child) = pipe.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(writer) = pipe.writer.take() {
            let _ = writer.join();
        }
        let _ = std::fs::remove_file(&pipe.out);
        if let Some(dir) = pipe.out.parent() {
            cleanup_frames(dir);
        }
    }
}

fn spawn_pipe(
    cfg: &RenderPipeConfig,
    width: u32,
    height: u32,
    bgra: bool,
) -> Result<RenderPipe, String> {
    let out = cfg.dir.join(format!("{}.mp4", sanitize_demo_name(&cfg.name)));
    let mut cmd = ffmpeg_command(cfg, width, height, bgra, &out);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    diag::info!(World, "Skate encode streaming: {:?}", cmd);
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg: {e}"))?;
    let Some(stdin) = child.stdin.take() else {
        return Err("ffmpeg stdin unavailable".into());
    };
    // A few frames of slack: enough to keep the encoder fed without letting
    // memory grow during long renders (capacity 4 is ~132 MB at 4K).
    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(4);
    let writer = std::thread::Builder::new()
        .name("skate-render-writer".into())
        .spawn(move || {
            let mut stdin = stdin;
            while let Ok(frame) = receiver.recv() {
                if stdin.write_all(&frame).is_err() {
                    break;
                }
            }
            let _ = stdin.flush();
        })
        .map_err(|e| e.to_string())?;
    Ok(RenderPipe {
        sender: Some(sender),
        writer: Some(writer),
        child: Some(child),
        out,
    })
}

/// Deletes raw frame caches left behind by earlier builds or cancelled
/// renders. Streaming renders never create these.
fn sweep_stale_render_caches() {
    let root = PathBuf::from("iw4l-artifacts").join("render");
    let Ok(dirs) = std::fs::read_dir(&root) else {
        return;
    };
    for dir in dirs.flatten() {
        cleanup_frames(&dir.path());
    }
}

/// The frame size the cinematic bars are computed against: the `WxH` preset
/// if one is set, otherwise the window's own frame.
fn cinematic_target(scale: &str, width: u32, height: u32) -> (u32, u32) {
    if let Some((w, h)) = scale.split_once('x')
        && let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>())
    {
        return (w, h);
    }
    (width, height)
}

fn cleanup_frames(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "raw") {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Clone, Copy)]
enum Encoder {
    Nvenc(&'static str),
    Amf(&'static str),
    X265,
}

/// The encoder renders use, worked out once per run. The first call blocks
/// on the probe; `warm_encoder` starts it early so a render never waits.
fn pick_encoder() -> Encoder {
    static PICKED: std::sync::OnceLock<Encoder> = std::sync::OnceLock::new();
    *PICKED.get_or_init(probe_encoder)
}

/// Starts the encoder probe in the background (when a replay opens).
fn warm_encoder() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("skate-encoder-probe".into())
            .spawn(|| {
                pick_encoder();
            });
    });
}

fn probe_encoder() -> Encoder {
    let Ok(output) = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
    else {
        return Encoder::X265;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    // Match encoder names as whole tokens (robust to column alignment).
    for (encoder, kind) in [
        ("hevc_nvenc", Encoder::Nvenc("hevc_nvenc")),
        ("h264_nvenc", Encoder::Nvenc("h264_nvenc")),
        ("hevc_amf", Encoder::Amf("hevc_amf")),
        ("h264_amf", Encoder::Amf("h264_amf")),
    ] {
        // Most ffmpeg builds list every hardware encoder whether or not the
        // PC has that GPU, so a listed one is only used once a tiny test
        // encode on it works. Otherwise an AMD PC picks NVENC and every
        // render fails at the end.
        if text.split_whitespace().any(|w| w == encoder) && encoder_works(encoder) {
            diag::info!(World, "Skate encode: {encoder} works on this PC");
            return kind;
        }
    }
    Encoder::X265
}

fn encoder_works(name: &str) -> bool {
    std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=256x256:r=30",
            "-frames:v",
            "3",
            "-c:v",
            name,
            "-f",
            "null",
            "-",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The ffmpeg command that reads raw frames from stdin and writes the encoded
/// video. Input filters (motion blur, the TikTok crop, output scaling) run
/// here, exactly as they did over the old raw cache.
fn ffmpeg_command(
    cfg: &RenderPipeConfig,
    width: u32,
    height: u32,
    bgra: bool,
    out: &std::path::Path,
) -> std::process::Command {
    // The pipe carries the captured swap-chain pixels as they are: four
    // bytes a pixel, in the swap chain's own channel order.
    let pix_fmt = if bgra { "bgra" } else { "rgba" };
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostats")
        .arg("-framerate")
        .arg(format!("{}", cfg.fps * cfg.shutter as f32))
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg(pix_fmt)
        .arg("-s")
        .arg(format!("{width}x{height}"))
        .arg("-i")
        .arg("pipe:0")
        .arg("-r")
        .arg(format!("{}", cfg.fps));
    let mut filters: Vec<String> = Vec::new();
    if cfg.shutter > 1 {
        filters.push(format!("tmix=frames={}", cfg.shutter));
    }
    if cfg.portrait {
        // TikTok template: keep only the 9:16 guide frame (centred, clamped
        // so a narrower window can't over-crop) and scale it to 1080x1920.
        filters.push("crop=min(iw\\,ih*9/16):min(ih\\,iw*16/9)".into());
        filters.push("scale=1080:1920:flags=lanczos".into());
    } else {
        if !cfg.scale.is_empty() {
            filters.push(format!("scale={}:flags=lanczos", cfg.scale));
        }
        if cfg.cinematic {
            // Cinematic template: crop to the 2.39:1 band, then bake black
            // bars back on, so the video is letterboxed exactly like the
            // on-screen bars show. Dimensions follow the scaled frame.
            let (tw, th) = cinematic_target(&cfg.scale, width, height);
            let crop_h = (((tw as f32) / CINEMATIC_ASPECT) as u32).max(2) & !1;
            let y = ((th.saturating_sub(crop_h)) / 2) & !1;
            filters.push(format!("crop={tw}:{crop_h}:0:{y}"));
            filters.push(format!("pad={tw}:{th}:0:{y}:black"));
        }
    }
    if !filters.is_empty() {
        cmd.arg("-vf").arg(filters.join(","));
    }
    let encoder = pick_encoder();
    let enc_name = match &encoder {
        Encoder::Nvenc(n) | Encoder::Amf(n) => *n,
        Encoder::X265 => "libx265",
    };
    diag::info!(World, "Skate encode encoder: {enc_name}");
    match encoder {
        Encoder::Nvenc(name) => {
            cmd.arg("-c:v")
                .arg(name)
                .arg("-cq")
                .arg(format!("{}", cfg.crf))
                .arg("-preset")
                .arg("p4");
        }
        Encoder::Amf(name) => {
            cmd.arg("-c:v")
                .arg(name)
                .arg("-rc")
                .arg("cqp")
                .arg("-qp_i")
                .arg(format!("{}", cfg.crf))
                .arg("-qp_p")
                .arg(format!("{}", cfg.crf));
        }
        Encoder::X265 => {
            cmd.arg("-c:v")
                .arg("libx265")
                .arg("-crf")
                .arg(format!("{}", cfg.crf))
                .arg("-preset")
                .arg("medium");
        }
    }
    // HEVC in MP4 needs the hvc1 tag for Apple players, iPhones and most
    // upload sites to accept it; faststart puts the index up front so the
    // video starts playing before it has fully downloaded.
    if matches!(enc_name, "hevc_nvenc" | "hevc_amf" | "libx265") {
        cmd.arg("-tag:v").arg("hvc1");
    }
    cmd.arg("-movflags").arg("+faststart");
    cmd.arg("-pix_fmt").arg("yuv420p").arg(out);
    cmd
}

fn render_step(
    mode: &mut SkateMode,
    host: &mut Host,
    authority: &mut net::AuthorityWorld,
    commands: &mut Commands,
    render: &mut SkateRenderState,
) {
    if render.capturing {
        return;
    }
    // Every frame is captured: close the video only now, once the last
    // screenshot has reached the encoder (closing on the frame that asked for
    // it dropped the final frame of every render).
    if render.frame >= render.total {
        let frames = render.frame;
        render.rendering = false;
        render.finished = true;
        mode.replaying = false;
        diag::info!(World, "Skate render complete: {frames} frames");
        render.finish_pipe();
        return;
    }
    let fps = mode.render_fps.max(1.0);
    let shutter = mode.render_shutter.max(1);
    // Video seconds between two captured frames: the render's own clock, so
    // smoothed cameras move the same however long each capture takes.
    let step = 1.0 / (fps * shutter as f32);
    let last = host.recorded.last().map(|f| f.time_secs).unwrap_or(0.0);
    let out_time = if mode.trim_end > 0.0 {
        mode.trim_end.clamp(0.0, last)
    } else {
        last
    };

    // Show the frame at the playhead, then advance: the first frame of the
    // video is the trim in itself.
    let t = host.replay_clock_secs;
    present_at(mode, host, t, authority);
    if !host.recorded.is_empty() {
        let eff_cam = active_cut(&host.cuts, t).unwrap_or(mode.replay_cam);
        // The render plays the saved free-cam and tripod shots; the pad and
        // keyboard can't nudge the camera mid-render.
        enter_placed_shot(host, mode, t, eff_cam, false);
        let heading = replay_heading(mode);
        apply_replay_camera(mode, t, heading, host, step);
        host.last_view = mode.camera;
    }
    let ramp = clip_speed(&host.speed_ramps, mode.replay_speed, t);
    host.replay_clock_secs = (t + step * ramp).min(out_time);

    commands
        .spawn(Screenshot::primary_window())
        .observe(move |captured: On<ScreenshotCaptured>, mut r: ResMut<SkateRenderState>| {
            let image = &captured.event().image;
            r.width = image.width();
            r.height = image.height();
            r.bgra = matches!(
                image.texture_descriptor.format,
                bevy::render::render_resource::TextureFormat::Bgra8Unorm
                    | bevy::render::render_resource::TextureFormat::Bgra8UnormSrgb
            );
            if let Err(e) = r.feed_frame(image) {
                r.pipe_error = Some(e);
            }
            r.capturing = false;
        });
    render.capturing = true;
    render.frame += 1;
}

fn update(
    time: Res<Time>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    screen: Res<AppScreen>,
    local: Res<net::LocalPresentClient>,
    presented: Res<net::PresentedSnapshot>,
    clip: Res<crate::DynEntPhysClip>,
    keys: Res<ButtonInput<KeyCode>>,
    mut ui_draw: ResMut<frame::UiDraw>,
    mut authority: Option<ResMut<net::AuthorityWorld>>,
    mut mode: ResMut<SkateMode>,
    mut host: ResMut<Host>,
    mut commands: Commands,
    mut render: ResMut<SkateRenderState>,
    (gamepads, pads, active): (
        Query<Entity, With<Gamepad>>,
        Query<&bevy::input::gamepad::Gamepad>,
        Option<Res<frame::ActivePad>>,
    ),
    mut rumble: MessageWriter<GamepadRumbleRequest>,
    mut wheel: MessageReader<MouseWheel>,
) {
    // The live MW2 HUD shows the match, not the replay, and would sit under
    // the replay dock: keep it off while a replay or render is on screen and
    // hand it back afterwards.
    let replay_view = mode.active && (mode.replaying || mode.rendering);
    if replay_view != host.hud_suppressed {
        host.hud_suppressed = replay_view;
        ui_draw.0 = !replay_view && !mode.hide_hud;
    }
    if let Some(name) = std::mem::take(&mut mode.save_requested) {
        if host.saving.is_some() {
            toast(&mut mode, "Still saving the last clip, try again in a moment");
        } else {
            match demo_file(&host, (mode.trim_start, mode.trim_end)) {
                Ok(file) => {
                    diag::info!(
                        World,
                        "Skate demo saving: {name} ({} frames, {} cuts, {} keyframes)",
                        host.recorded.len(),
                        host.cuts.len(),
                        host.keyframes.len()
                    );
                    let (done, result) = mpsc::channel();
                    let spawned = std::thread::Builder::new()
                        .name("skate-save".into())
                        .spawn(move || {
                            let _ = done.send((name.clone(), write_demo(&file, &name)));
                        });
                    match spawned {
                        Ok(_) => {
                            host.saving = Some(Mutex::new(result));
                            toast(&mut mode, "Saving...");
                        }
                        Err(e) => toast(&mut mode, format!("Save failed: {e}")),
                    }
                }
                Err(e) => {
                    diag::warn!(World, "Skate save failed: {e}");
                    toast(&mut mode, format!("Save failed: {e}"));
                }
            }
        }
    }
    let finished = host.saving.as_ref().map(|r| r.lock().unwrap_or_else(|e| e.into_inner()).try_recv());
    match finished {
        Some(Ok((name, Ok(path)))) => {
            host.saving = None;
            diag::info!(World, "Skate demo saved to {}", path.display());
            toast(&mut mode, format!("Saved as {name}"));
        }
        Some(Ok((_, Err(e)))) => {
            host.saving = None;
            diag::warn!(World, "Skate save failed: {e}");
            toast(&mut mode, format!("Save failed: {e}"));
        }
        Some(Err(mpsc::TryRecvError::Disconnected)) => {
            host.saving = None;
            toast(&mut mode, "Save failed: the save worker stopped");
        }
        Some(Err(mpsc::TryRecvError::Empty)) | None => {}
    }
    if let Some(name) = std::mem::take(&mut mode.load_requested) {
        match load_demo(&name) {
            Ok(loaded) => {
                let count = loaded.frames.len();
                host.recorded = loaded.frames;
                host.names = loaded.names;
                mode.names.clear();
                host.replay_index = 0;
                host.cuts = loaded.cuts;
                host.keyframes = loaded.keyframes;
                host.speed_ramps = loaded.ramps;
                host.keyframe_linear = !loaded.ease;
                host.free_base = loaded.free_base;
                host.prev_eff_cam = u8::MAX;
                clear_history(&mut host);
                let length = host.recorded.last().map(|f| f.time_secs).unwrap_or(0.0);
                mode.trim_start = loaded.trim.0.clamp(0.0, length);
                mode.trim_end = loaded.trim.1.clamp(0.0, length);
                if mode.replaying {
                    // Loaded from the replay menu: start the new clip paused
                    // at its trim in, with the editor out of the way.
                    host.replay_clock_secs = mode.trim_start;
                    host.follow_init = false;
                    host.tripod_init = false;
                    host.grab = None;
                    host.mouse_drag = None;
                    host.drag_base = None;
                    mode.replay_paused = true;
                    mode.menu_open = false;
                    mode.load_open = false;
                    toast(&mut mode, format!("Loaded {name} ({length:.1} s)"));
                }
                diag::info!(
                    World,
                    "Skate demo loaded: {name} ({count} frames, {} cuts, {} keyframes, ease {})",
                    host.cuts.len(),
                    host.keyframes.len(),
                    if host.keyframe_linear { "linear" } else { "smooth" }
                );
            }
            Err(e) => diag::warn!(World, "Skate load failed: {e}"),
        }
    }
    let Some(authority) = authority.as_deref_mut() else {
        return;
    };
    let aspect_ratio = windows
        .single()
        .map(|w| w.width() / w.height().max(1.))
        .unwrap_or(16. / 9.);
    let ps = presented.player(local.0);
    let alive = ps.is_some_and(|p| p.pm_type == 0) && *screen == AppScreen::InGame;
    let same_map = host
        .clip
        .as_ref()
        .is_none_or(|a| clip.0.as_ref().is_some_and(|b| Arc::ptr_eq(a, b)));
    if (mode.active || host.enter_requested || host.activating) && (!alive || !same_map) {
        stop(&mut host, &mut mode, authority);
    }
    if !same_map {
        host.send = None;
        host.receive = None;
        host.clip = None;
        host.ready = false;
        host.recorded.clear();
        host.rolling.clear();
        host.record_clock = 0.0;
        host.replay_index = 0;
        host.replay_clock_secs = 0.0;
        mode.recording = false;
        mode.replaying = false;
        mode.replay_paused = false;
        mode.replay_speed = 1.0;
        mode.preloaded = false;
        mode.preload_pending = std::env::var_os("IW4L_SKATE_ASSETS").is_some();
    }
    // This runs during map preparation/class selection, without waiting for J.
    if host.clip.is_none()
        && std::env::var_os("IW4L_SKATE_ASSETS").is_some()
        && let Some(geometry) = clip.0.clone()
    {
        mode.preload_pending = true;
        host.clip = Some(geometry.clone()); // A failed load retries on a new map, never every frame.
        if let Err(e) = preload_map(&mut host, geometry) {
            diag::warn!(World, "Skate map preload: {e}");
            mode.preload_pending = false;
            mode.status = e;
        }
    }
    // Skating reads the same controller as the rest of the game, whatever
    // kind it is, converted to the Xbox layout the skate input expects.
    let pad = active.and_then(|active| active.0).and_then(|entity| pads.get(entity).ok());
    host.pad_packet = host.pad_packet.wrapping_add(1);
    let input = pad.map_or_else(InputFrame::neutral, |pad| pad_frame(pad, host.pad_packet));
    mode.controller = input.controller();
    let buttons = input.buttons();
    let pressed = buttons & !host.previous_buttons;
    let released = host.previous_buttons & !buttons;
    host.previous_buttons = buttons;

    const BTN_START: u16 = 0x10;
    const BTN_BACK: u16 = 0x20;
    const BTN_DPAD_U: u16 = 0x01;
    const BTN_DPAD_D: u16 = 0x02;
    const BTN_DPAD_L: u16 = 0x04;
    const BTN_DPAD_R: u16 = 0x08;
    const BTN_LB: u16 = 0x100;
    const BTN_RB: u16 = 0x200;
    const BTN_LSTICK: u16 = 0x40;
    const BTN_RSTICK: u16 = 0x80;
    const BTN_A: u16 = 0x1000;
    const BTN_B: u16 = 0x2000;
    const BTN_X: u16 = 0x4000;
    const BTN_Y: u16 = 0x8000;

    if mode.active {
        // In a replay RS click is camera control and LS click hides the HUD
        // (handled with the replay controls below).
        if !mode.rendering
            && (keys.just_pressed(KeyCode::KeyH) || (!mode.replaying && pressed & BTN_RSTICK != 0))
        {
            mode.hide_hud = !mode.hide_hud;
            ui_draw.0 = !mode.hide_hud && !host.hud_suppressed;
        }
        // Live skating only; the replay has its own layout below.
        if !mode.replaying {
            // Start opens the game's own pause menu (console gamepad input).
            if pressed & BTN_BACK != 0 {
                if buttons & BTN_RB != 0 {
                    // RB+Back: instant replay of the last seconds of skating.
                    mode.instant_requested = true;
                } else if buttons & BTN_LB != 0 {
                    // LB+Back: the recording, or the instant replay when
                    // nothing was recorded.
                    if host.recorded.is_empty() && !mode.recording {
                        mode.instant_requested = true;
                    } else {
                        mode.replay_requested = true;
                    }
                } else {
                    mode.record_requested = true;
                }
            }
        }
    }

    let mut replies = Vec::new();
    if let Some(receiver) = &host.receive {
        let receiver = receiver.lock().unwrap();
        loop {
            match receiver.try_recv() {
                Ok(reply) => replies.push(reply),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    replies.push(Reply::Error("Skate worker disconnected".into()));
                    break;
                }
            }
        }
    }
    for reply in replies {
        match reply {
            Reply::Ready => {
                host.ready = true;
                mode.preloaded = true;
                mode.preload_pending = false;
                diag::info!(World, "Skate ready before toggle");
            }
            Reply::Activated(epoch, p, ms) if epoch == host.epoch && host.activating && alive => {
                host.activating = false;
                mode.entering = false;
                mode.active = true;
                host.input_suspended = false;
                authority.0.set_external_motion(local.0, true);
                if host.names.is_empty() && !p.names.is_empty() {
                    host.names = p.names.clone();
                }
                present(&mut mode, &p, &host.names, authority);
                diag::info!(World, "Skate activation from retained session: {ms}ms");
            }
            Reply::Pose(epoch, p) if epoch == host.epoch && mode.active => {
                if p.tick / 120 != host.logged_tick / 120 {
                    diag::info!(
                        World,
                        "Skate tick={} speed={:.2} state={}",
                        p.tick,
                        p.velocity.length(),
                        p.state
                    );
                    host.logged_tick = p.tick;
                }
                // Seed the shared skeleton names before the first present.
                if host.names.is_empty() && !p.names.is_empty() {
                    host.names = p.names.clone();
                }
                present(&mut mode, &p, &host.names, authority);
                // Instant replay: keep the last INSTANT_SECS of skating. The
                // clock only advances by a frame's worth, so pauses and menus
                // leave no dead air, and expired frames are reused so the
                // buffer doesn't allocate once it is full.
                // At most 120 poses a second: with vsync off the game can
                // run far faster, and 45 s of every frame would waste memory.
                let now = time.elapsed_secs();
                if host.rolling.is_empty() || now - host.rolling_last >= 1.0 / 120.0 {
                    let step = if host.rolling.is_empty() {
                        0.0
                    } else {
                        (now - host.rolling_last).clamp(0.0, 0.1)
                    };
                    host.rolling_last = now;
                    host.rolling_clock += step;
                    let clock = host.rolling_clock;
                    let reuse = host
                        .rolling
                        .front()
                        .is_some_and(|f| f.time_secs < clock - INSTANT_SECS);
                    let mut slot = if reuse {
                        host.rolling.pop_front()
                    } else {
                        None
                    }
                    .unwrap_or_else(|| RecordedFrame {
                        pose: Pose {
                            root: Mat4::IDENTITY,
                            bones: Vec::new(),
                            names: Vec::new(),
                            camera: None,
                            velocity: Vec3::ZERO,
                            tick: 0,
                            state: String::new(),
                        },
                        time_secs: 0.0,
                    });
                    slot.pose.root = p.root;
                    slot.pose.bones.clear();
                    slot.pose.bones.extend_from_slice(&p.bones);
                    slot.pose.camera = p.camera;
                    slot.pose.velocity = p.velocity;
                    slot.pose.tick = p.tick;
                    slot.time_secs = clock;
                    host.rolling.push_back(slot);
                    while host.rolling.front().is_some_and(|f| f.time_secs < clock - INSTANT_SECS) {
                        host.rolling.pop_front();
                    }
                }
                if mode.recording {
                    // Move the pose into the recording and drop the per-frame
                    // name list: `host.names` carries the skeleton now. The
                    // clock steps by at most 0.1 s, so time spent paused or
                    // in a menu mid-recording isn't replayed as a frozen
                    // skater.
                    let now = time.elapsed_secs();
                    if !host.recorded.is_empty() {
                        host.record_clock += (now - host.record_last).clamp(0.0, 0.1);
                    }
                    host.record_last = now;
                    let at_secs = host.record_clock;
                    let mut pose = p;
                    pose.names = Vec::new();
                    host.recorded.push(RecordedFrame {
                        pose,
                        time_secs: at_secs,
                    });
                }
            }
            Reply::Error(e) => {
                stop(&mut host, &mut mode, authority);
                host.send = None;
                host.receive = None;
                host.ready = false;
                mode.preloaded = false;
                mode.preload_pending = false;
                // The worker is gone; prepare a fresh session for this map
                // so the player can skate again rather than for the rest of
                // the match being left without one.
                host.clip = None;
                diag::warn!(World, "Skate stopped: {e}; preparing a new session");
                mode.status = e;
                return;
            }
            _ => {}
        }
    }
    if std::mem::take(&mut mode.toggle_requested) && alive {
        if mode.active || host.enter_requested || host.activating {
            stop(&mut host, &mut mode, authority);
            return;
        }
        if host.send.is_none() {
            diag::warn!(World, "Skate session unavailable: {}", mode.status);
            return;
        }
        host.enter_requested = true;
        mode.entering = true;
        mode.client = local.0.0;
    }
    if host.enter_requested
        && host.ready
        && let Some(ps) = ps.filter(|_| alive)
    {
        host.epoch = host.epoch.wrapping_add(1);
        host.enter_requested = false;
        host.activating = true;
        if let Some(send) = &host.send {
            let _ = send.send(Job::Activate(
                host.epoch,
                Vec3::from_array(ps.origin) + Vec3::Z * 2.,
                ps.viewangles[1],
                aspect_ratio,
            ));
        }
    }
    if !mode.active {
        return;
    }
    if std::mem::take(&mut mode.record_requested) {
        if mode.recording {
            mode.recording = false;
            diag::info!(World, "Skate recording stopped ({} frames)", host.recorded.len());
        } else {
            host.recorded.clear();
            host.keyframes.clear();
            host.cuts.clear();
            host.speed_ramps.clear();
            host.free_base = None;
            clear_history(&mut host);
            host.names.clear();
            mode.names.clear();
            mode.trim_start = 0.0;
            mode.trim_end = 0.0;
            host.record_clock = 0.0;
            host.record_last = time.elapsed_secs();
            mode.recording = true;
            mode.replaying = false;
            diag::info!(World, "Skate recording started");
        }
    }
    // During a replay, C goes through the same cut-aware cycling as LB/RB.
    if !mode.replaying && std::mem::take(&mut mode.cam_requested) {
        mode.replay_cam = (mode.replay_cam + 1) % REPLAY_CAM_COUNT;
        diag::info!(World, "Skate replay camera: {}", replay_cam_name(mode.replay_cam));
    }
    if let Some(cam) = mode.cam_set_requested.take() {
        if cam < REPLAY_CAM_COUNT {
            mode.replay_cam = cam;
            diag::info!(World, "Skate replay camera: {}", replay_cam_name(cam));
        } else {
            diag::warn!(World, "Skate cam: {cam} is out of range (0..{})", REPLAY_CAM_COUNT - 1);
        }
    }
    if let Some(ease) = mode.ease_requested.take() {
        host.keyframe_linear = !ease;
        diag::info!(World, "Skate keyframe easing: {}", if ease { "smooth" } else { "linear" });
    }
    if std::mem::take(&mut mode.keyframe_clear_requested) {
        host.keyframes.clear();
        diag::info!(World, "Skate keyframes cleared");
    }
    if std::mem::take(&mut mode.cut_clear_requested) {
        host.cuts.clear();
        diag::info!(World, "Skate cuts cleared");
    }
    if std::mem::take(&mut mode.instant_requested) && !mode.replaying && !mode.rendering {
        if mode.recording {
            // Recording already: replay that instead.
            mode.recording = false;
            mode.replay_requested = true;
        } else if host.rolling.len() < 2 {
            diag::warn!(World, "Skate instant replay: nothing skated yet");
        } else {
            let start = host.rolling.front().map(|f| f.time_secs).unwrap_or(0.0);
            host.recorded = host
                .rolling
                .iter()
                .map(|f| RecordedFrame { pose: f.pose.clone(), time_secs: f.time_secs - start })
                .collect();
            host.keyframes.clear();
            host.cuts.clear();
            host.speed_ramps.clear();
            host.free_base = None;
            clear_history(&mut host);
            mode.trim_start = 0.0;
            mode.trim_end = 0.0;
            mode.replay_requested = true;
            diag::info!(
                World,
                "Skate instant replay: last {:.1}s ({} frames)",
                host.recorded.last().map(|f| f.time_secs).unwrap_or(0.0),
                host.recorded.len()
            );
        }
    }
    if std::mem::take(&mut mode.replay_requested) {
        if mode.replaying {
            mode.replaying = false;
            mode.menu_open = false;
            mode.hold = None;
            mode.cam_wheel = None;
            mode.quick_layer = false;
            mode.delete_target = None;
            mode.prompts.clear();
            mode.load_open = false;
            mode.grabbed = None;
            host.grab = None;
            host.mouse_drag = None;
            host.drag_base = None;
            diag::info!(World, "Skate replay stopped");
        } else if host.recorded.is_empty() {
            diag::warn!(World, "Skate replay: nothing recorded");
        } else {
            mode.recording = false;
            mode.replaying = true;
            mode.replay_paused = false;
            mode.replay_speed = 1.0;
            mode.replay_fov = 55.0;
            mode.replay_roll = 0.0;
            mode.menu_open = false;
            mode.menu_focus = 0;
            mode.toast = None;
            mode.hold = None;
            // Render settings start usable even if `skate render` never ran.
            if mode.render_fps <= 0.0 {
                mode.render_fps = 60.0;
            }
            if mode.render_shutter == 0 {
                mode.render_shutter = 1;
            }
            if mode.render_crf == 0 {
                mode.render_crf = 18;
            }
            if mode.render_name.is_empty() {
                mode.render_name = "clip".into();
            }
            host.replay_index = 0;
            host.replay_clock_secs = mode.trim_start.max(0.0);
            host.follow_init = false;
            host.tripod_init = false;
            host.prev_eff_cam = u8::MAX;
            host.last_view = None;
            host.locked_yaw = 0.0;
            host.locked_pitch = 0.0;
            host.locked_dist = 0.0;
            host.back_armed = false;
            host.hold_exit = Hold::default();
            host.hold_delete = Hold::default();
            host.hold_menu = Hold::default();
            host.x_down = false;
            host.rb_down = false;
            host.rb_used = false;
            host.stick_combo = false;
            mode.cam_wheel = None;
            mode.quick_layer = false;
            mode.load_open = false;
            mode.grabbed = None;
            host.grab = None;
            host.mouse_drag = None;
            host.drag_base = None;
            host.b_secs = 0.0;
            mode.cam_control = frame::replay_cam_steerable(mode.replay_cam);
            if !host.tip_shown {
                host.tip_shown = true;
                let tip = if mode.pad_prompts || mode.controller.is_some() {
                    "Tip: hold X to pick a camera, hold RB for quick edits, Back for every control"
                } else {
                    "Tip: 1-0 cut to a camera, Ctrl+Z undoes, / shows every control"
                };
                toast_for(&mut mode, tip, 6.0);
            }
            // Renders start from here: find the working encoder now, in the
            // background, so pressing Render never waits on the probe.
            warm_encoder();
            diag::info!(World, "Skate replay started ({} frames)", host.recorded.len());
        }
    }
    if std::mem::take(&mut mode.render_requested) {
        if host.recorded.is_empty() {
            diag::warn!(World, "Skate render: nothing recorded");
        } else {
            let duration = host.recorded.last().map(|f| f.time_secs).unwrap_or(0.0);
            let in_time = mode.trim_start.clamp(0.0, duration);
            let out_time = if mode.trim_end > 0.0 {
                mode.trim_end.clamp(in_time, duration)
            } else {
                duration
            };
            let fps = mode.render_fps.max(1.0);
            let shutter = mode.render_shutter.max(1);
            let dir = render_dir(&mode.render_name);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                diag::warn!(World, "Skate render: {e}");
            } else {
                render.dir = dir.clone();
                render.pipe = None;
                render.pipe_error = None;
                sweep_stale_render_caches();
                render.pipe_cfg = Some(RenderPipeConfig {
                    fps,
                    shutter,
                    crf: mode.render_crf,
                    scale: mode.render_scale.clone(),
                    portrait: mode.portrait,
                    cinematic: mode.cinematic,
                    name: mode.render_name.clone(),
                    dir,
                });
                let step = 1.0 / (fps * shutter as f32);
                let mut total = 0.0f32;
                let mut t = in_time;
                let max_frames = ((out_time - in_time) * fps * shutter as f32 * 8.0).max(1.0);
                while t < out_time && total < max_frames {
                    t += step * clip_speed(&host.speed_ramps, mode.replay_speed, t);
                    total += 1.0;
                }
                render.total = total.ceil().max(1.0) as u32;
                render.frame = 0;
                render.capturing = false;
                render.rendering = true;
                // The first rendered frame restores its free-cam shot.
                host.prev_eff_cam = u8::MAX;
                host.replay_index = 0;
                host.replay_clock_secs = in_time;
                host.follow_init = false;
                host.tripod_init = false;
                mode.recording = false;
                mode.replaying = true;
                mode.replay_paused = false;
                mode.rendering = true;
                mode.hide_hud = true;
                ui_draw.0 = false;
                diag::info!(
                    World,
                    "Skate render: {} frames @ {:.0}fps shutter={} into {}",
                    render.total,
                    fps,
                    shutter,
                    render.dir.display()
                );
            }
        }
    }
    if render.rendering {
        let failed = render.pipe_error.take();
        // Esc, or hold B on the pad for a second, cancels the render.
        let pad_cancel = host.hold_render_cancel.tick(buttons & BTN_B != 0, time.delta_secs(), 1.0);
        if keys.just_pressed(KeyCode::Escape) || pad_cancel || failed.is_some() {
            host.hold_render_cancel = Hold::default();
            render.abort_pipe();
            render.rendering = false;
            render.capturing = false;
            render.finished = false;
            mode.replaying = false;
            mode.rendering = false;
            mode.hide_hud = false;
            ui_draw.0 = true;
            if let Some(e) = failed {
                toast(&mut mode, format!("Render stopped: {e}"));
            } else {
                diag::info!(World, "Skate render cancelled");
            }
            return;
        }
        render_step(&mut mode, &mut host, authority, &mut commands, &mut render);
        return;
    }
    if render.finished && !render.capturing {
        render.finished = false;
        mode.rendering = false;
        mode.hide_hud = false;
        ui_draw.0 = true;
    }
    if mode.replaying {
        let dt = time.delta_secs().min(0.1);
        let last_time = host.recorded.last().map(|f| f.time_secs).unwrap_or(0.0);
        let out_time = if mode.trim_end > 0.0 {
            mode.trim_end.clamp(0.0, last_time)
        } else {
            last_time
        };
        let in_time = mode.trim_start.clamp(0.0, out_time);
        // Timeline state before this frame's input, for undo.
        let edits_before = edit_snapshot(&host, &mode);
        host.history_applied = false;
        // The camera at the frame's start drives which controls are active.
        let cam_start = active_cut(&host.cuts, host.replay_clock_secs).unwrap_or(mode.replay_cam);
        let free = cam_start == 6;
        let locked_cam = cam_start == 12 || cam_start == 13;
        let steer = free || locked_cam;
        let lstick = input.left_stick();
        let rstick = input.right_stick();
        let trig = input.triggers();
        let mut pulse = false;
        let mut wheel_delta = 0.0f32;
        for ev in wheel.read() {
            wheel_delta += ev.y;
        }

        if let Some((_, left)) = mode.toast.as_mut() {
            *left -= dt;
        }
        if mode.toast.as_ref().is_some_and(|(_, left)| *left <= 0.0) {
            mode.toast = None;
        }
        mode.hold = None;

        // Prompts follow whichever device was used last.
        if pressed != 0
            || lstick[0].hypot(lstick[1]) > 0.5
            || rstick[0].hypot(rstick[1]) > 0.5
            || trig[0] > 0.5
            || trig[1] > 0.5
        {
            mode.pad_prompts = true;
        } else if keys.get_just_pressed().next().is_some() {
            mode.pad_prompts = false;
        }

        // Stick clicks act on release, and never when both sticks were
        // clicked together: that toggles skating.
        if buttons & (BTN_LSTICK | BTN_RSTICK) == (BTN_LSTICK | BTN_RSTICK) {
            host.stick_combo = true;
        }
        let ls_click = released & BTN_LSTICK != 0 && !host.stick_combo;
        let rs_click = released & BTN_RSTICK != 0 && !host.stick_combo;
        if buttons & (BTN_LSTICK | BTN_RSTICK) == 0 {
            host.stick_combo = false;
        }
        // LS click: hide / show the replay HUD.
        if ls_click {
            mode.hide_hud = !mode.hide_hud;
            ui_draw.0 = !mode.hide_hud && !host.hud_suppressed;
        }
        // RS click: camera control on / off in the cameras you steer.
        if rs_click && !mode.menu_open && mode.cam_wheel.is_none() {
            if steer {
                mode.cam_control = !mode.cam_control;
                if mode.cam_control {
                    toast(&mut mode, "Camera control: the sticks and triggers move the camera");
                } else {
                    toast(&mut mode, "Playback control: LS scrubs, LT / RT change speed");
                }
                pulse = true;
            } else {
                toast(&mut mode, "Camera control is for Free cam, Body cam and Board cam");
            }
        }
        if buttons & BTN_B == 0 {
            host.b_block = false;
        }

        // B in the load list goes back to the editor menu.
        if mode.menu_open && mode.load_open && pressed & BTN_B != 0 {
            mode.load_open = false;
            host.b_block = true;
        }
        // Start / Tab open and close the editor menu; B also closes it.
        if pressed & BTN_START != 0
            || keys.just_pressed(KeyCode::Tab)
            || (mode.menu_open && pressed & BTN_B != 0 && !host.b_block)
        {
            mode.menu_open = !mode.menu_open;
            mode.load_open = false;
            host.grab = None;
            mode.grabbed = None;
            host.hold_menu = Hold::default();
            host.hold_delete = Hold::default();
            host.x_down = false;
            host.rb_down = false;
            mode.cam_wheel = None;
            mode.quick_layer = false;
            host.b_block = buttons & BTN_B != 0;
        }

        // Directions shared by the menu and the playback controls. Arrow keys
        // only count outside the cameras where they aim the view.
        let arrows = (!free && !locked_cam) || mode.menu_open;
        let stick_dir = |v: f32| -> i8 {
            if v > 0.5 {
                1
            } else if v < -0.5 {
                -1
            } else {
                0
            }
        };
        let mut h_dir: i8 = 0;
        if buttons & BTN_DPAD_L != 0
            || (arrows && keys.pressed(KeyCode::ArrowLeft))
            || keys.pressed(KeyCode::Comma)
        {
            h_dir -= 1;
        }
        if buttons & BTN_DPAD_R != 0
            || (arrows && keys.pressed(KeyCode::ArrowRight))
            || keys.pressed(KeyCode::Period)
        {
            h_dir += 1;
        }
        let mut v_dir: i8 = 0;
        if buttons & BTN_DPAD_U != 0
            || (arrows && keys.pressed(KeyCode::ArrowUp))
            || keys.pressed(KeyCode::PageUp)
        {
            v_dir += 1;
        }
        if buttons & BTN_DPAD_D != 0
            || (arrows && keys.pressed(KeyCode::ArrowDown))
            || keys.pressed(KeyCode::PageDown)
        {
            v_dir -= 1;
        }
        if mode.menu_open {
            if h_dir == 0 {
                h_dir = stick_dir(lstick[0]);
            }
            if v_dir == 0 {
                v_dir = stick_dir(lstick[1]);
            }
        }
        let h_step = host.rep_h.tick(h_dir, dt);
        let v_step = host.rep_v.tick(v_dir, dt);

        // Set when the player switches cameras themselves this frame, so
        // camera control can follow the new camera.
        let mut user_cam_change = false;
        // RB held: the quick-edit layer owns the face buttons, D-pad and LS.
        let mut layer = false;
        // X held long enough: the camera wheel owns RS and the D-pad.
        let mut wheel_active = false;
        let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
        let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);

        if mode.menu_open && mode.load_open {
            mode.cam_requested = false;
            let n = mode.load_entries.len();
            if n == 0 {
                mode.load_open = false;
            } else {
                if v_step != 0 {
                    mode.load_focus = (mode.load_focus as i32 - v_step as i32).rem_euclid(n as i32) as usize;
                }
                let mut pick = (pressed & BTN_A != 0 || keys.just_pressed(KeyCode::Enter))
                    .then_some(mode.load_focus);
                if let Some(row) = mode.load_click.take()
                    && row < n
                {
                    mode.load_focus = row;
                    pick = Some(row);
                }
                mode.load_focus = mode.load_focus.min(n - 1);
                if let Some(row) = pick {
                    mode.load_requested = Some(mode.load_entries[row].0.clone());
                }
            }
        } else if mode.menu_open {
            mode.cam_requested = false;
            let n = REPLAY_MENU.len();
            let mut activate = pressed & BTN_A != 0 || keys.just_pressed(KeyCode::Enter);
            let mut adjust = h_step;
            if v_step != 0 {
                // Up moves toward the top of the list.
                mode.menu_focus =
                    (mode.menu_focus as i32 - v_step as i32).rem_euclid(n as i32) as usize;
            }
            // LB / RB jump between the EDIT, CAMERA and RENDER sections.
            let starts: Vec<usize> = REPLAY_MENU
                .iter()
                .enumerate()
                .filter(|(_, item)| item.section().is_some())
                .map(|(i, _)| i)
                .collect();
            if pressed & BTN_RB != 0 && !starts.is_empty() {
                mode.menu_focus = starts
                    .iter()
                    .copied()
                    .find(|&s| s > mode.menu_focus)
                    .unwrap_or(starts[0]);
            }
            if pressed & BTN_LB != 0 && !starts.is_empty() {
                let here = starts.iter().copied().filter(|&s| s <= mode.menu_focus).max();
                mode.menu_focus = match here {
                    Some(s) if s < mode.menu_focus => s,
                    _ => starts
                        .iter()
                        .copied()
                        .filter(|&s| s < mode.menu_focus)
                        .max()
                        .unwrap_or(starts[starts.len() - 1]),
                };
            }
            if let Some((row, d)) = mode.menu_click.take()
                && row < n
            {
                mode.menu_focus = row;
                if d == 0 {
                    activate = true;
                } else {
                    adjust = d;
                }
            }
            mode.menu_focus = mode.menu_focus.min(n - 1);
            let item = REPLAY_MENU[mode.menu_focus];
            match item.kind() {
                ReplayMenuKind::Adjust => {
                    let d = if adjust != 0 {
                        adjust
                    } else if activate {
                        1
                    } else {
                        0
                    };
                    if d != 0 {
                        menu_adjust(item, d, &mut mode, &mut host);
                    }
                }
                ReplayMenuKind::Action => {
                    if activate {
                        menu_action(item, &mut mode, &mut host);
                    }
                }
                ReplayMenuKind::Hold => {
                    let down = buttons & BTN_A != 0
                        || keys.pressed(KeyCode::Enter)
                        || mode.menu_mouse_hold == Some(mode.menu_focus);
                    if host.hold_menu.tick(down, dt, HOLD_CONFIRM) {
                        menu_action(item, &mut mode, &mut host);
                        pulse = true;
                    }
                    if let Some(p) = host.hold_menu.progress(HOLD_CONFIRM) {
                        mode.hold = Some((format!("Hold: {}", item.label()), p));
                    }
                }
            }
        } else {
            host.hold_menu = Hold::default();
            let playing = !mode.replay_paused;

            // X on the pad: a tap cuts (as before), holding it opens the
            // camera wheel; releasing the wheel cuts to the camera picked.
            if pressed & BTN_X != 0 && !host.rb_down {
                host.x_down = true;
                host.x_secs = 0.0;
            }
            let x_held = host.x_down && buttons & BTN_X != 0;
            if x_held {
                host.x_secs += dt;
                if mode.cam_wheel.is_none() && host.x_secs >= 0.25 {
                    mode.cam_wheel = Some(cam_start);
                    pulse = true;
                }
            }
            if let Some(old) = mode.cam_wheel {
                wheel_active = true;
                let n = REPLAY_CAM_COUNT as i32;
                let mut pick = old;
                // RS points at a camera: slot 0 straight up, then clockwise.
                if rstick[0].hypot(rstick[1]) > 0.5 {
                    let tau = std::f32::consts::TAU;
                    let angle = rstick[0].atan2(rstick[1]).rem_euclid(tau);
                    pick = ((angle / tau * n as f32).round() as i32).rem_euclid(n) as u8;
                }
                if h_step != 0 {
                    pick = (pick as i32 + h_step as i32).rem_euclid(n) as u8;
                }
                if pick != old {
                    pulse = true;
                }
                mode.cam_wheel = Some(pick);
                if pressed & BTN_B != 0 {
                    mode.cam_wheel = None;
                    host.x_down = false;
                    host.b_block = true;
                    toast(&mut mode, "Camera pick cancelled");
                }
            }
            if host.x_down && buttons & BTN_X == 0 {
                host.x_down = false;
                let t = host.replay_clock_secs;
                match mode.cam_wheel.take() {
                    Some(pick) => {
                        mode.replay_cam = pick;
                        cut_to(&mut host, &mut mode, t, pick);
                    }
                    None => add_cut(&mut host, &mut mode, t),
                }
                user_cam_change = true;
                pulse = true;
            }

            // RB on the pad: a tap steps to the next camera, holding it is the
            // quick-edit layer.
            if pressed & BTN_RB != 0 && !wheel_active {
                host.rb_down = true;
                host.rb_secs = 0.0;
                host.rb_used = false;
            }
            let rb_held = host.rb_down && buttons & BTN_RB != 0;
            let mut rb_tap = false;
            if rb_held {
                host.rb_secs += dt;
            } else if host.rb_down {
                rb_tap = !host.rb_used && host.rb_secs < 0.35;
                host.rb_down = false;
            }
            layer = rb_held;
            mode.quick_layer = rb_held && (host.rb_secs >= 0.2 || host.rb_used);

            if layer {
                // Quick-edit layer: trim, ramps, zoom / roll, frames, undo.
                if pressed & BTN_DPAD_L != 0 {
                    menu_action(ReplayMenuItem::TrimIn, &mut mode, &mut host);
                    host.rb_used = true;
                    pulse = true;
                }
                if pressed & BTN_DPAD_R != 0 {
                    menu_action(ReplayMenuItem::TrimOut, &mut mode, &mut host);
                    host.rb_used = true;
                    pulse = true;
                }
                if pressed & BTN_DPAD_U != 0 {
                    menu_action(ReplayMenuItem::RampAdd, &mut mode, &mut host);
                    host.rb_used = true;
                    pulse = true;
                }
                if pressed & BTN_DPAD_D != 0 {
                    menu_action(ReplayMenuItem::TrimClear, &mut mode, &mut host);
                    host.rb_used = true;
                    pulse = true;
                }
                // LS: up / down zooms (up is tighter), left / right rolls.
                let ls = stick_curve(lstick);
                if ls[0] != 0.0 || ls[1] != 0.0 {
                    // The free cam keeps its own field of view.
                    if free {
                        mode.free_cam_fov = (mode.free_cam_fov - 40.0 * ls[1] * dt).clamp(10.0, 120.0);
                    } else {
                        mode.replay_fov = (cam_fov(&mode) - 40.0 * ls[1] * dt).clamp(10.0, 120.0);
                    }
                    mode.replay_roll = (mode.replay_roll + 0.8 * ls[0] * dt).clamp(-0.6, 0.6);
                    host.rb_used = true;
                }
                if pressed & BTN_X != 0 {
                    menu_adjust(ReplayMenuItem::Portrait, 1, &mut mode, &mut host);
                    let msg = if mode.portrait { "TikTok 9:16 frame on" } else { "TikTok 9:16 frame off" };
                    toast(&mut mode, msg);
                    host.rb_used = true;
                }
                if pressed & BTN_A != 0 {
                    menu_adjust(ReplayMenuItem::Cinematic, 1, &mut mode, &mut host);
                    let msg = if mode.cinematic { "Cinematic bars on" } else { "Cinematic bars off" };
                    toast(&mut mode, msg);
                    host.rb_used = true;
                }
                if pressed & BTN_B != 0 {
                    undo_edit(&mut host, &mut mode);
                    host.rb_used = true;
                    host.b_block = true;
                    pulse = true;
                }
                if pressed & BTN_Y != 0 {
                    redo_edit(&mut host, &mut mode);
                    host.rb_used = true;
                    pulse = true;
                }
            }

            // A / Space: play or pause; from the end, play restarts at trim in.
            let a_play = pressed & BTN_A != 0 && !layer && !wheel_active && host.grab.is_none();
            if a_play || keys.just_pressed(KeyCode::Space) {
                if mode.replay_paused && host.replay_clock_secs >= out_time - 1.0e-3 {
                    host.replay_clock_secs = in_time;
                }
                mode.replay_paused = !mode.replay_paused;
            }
            // LB / tapped RB / C: the camera under the playhead. While playing,
            // the switch itself stamps a cut, so hard cuts are one press.
            if pressed & BTN_LB != 0
                && !layer
                && !wheel_active
                && cycle_camera(&mut host, &mut mode, -1, playing)
            {
                pulse = true;
            }
            if pressed & BTN_LB != 0 && !layer && !wheel_active {
                user_cam_change = true;
            }
            let cam_req = std::mem::take(&mut mode.cam_requested);
            if rb_tap || cam_req {
                if cycle_camera(&mut host, &mut mode, 1, playing) {
                    pulse = true;
                }
                user_cam_change = true;
            }
            if !layer && !wheel_active {
                // D-pad left/right, comma/period: one recorded frame at a time.
                if h_step != 0 && !host.recorded.is_empty() {
                    mode.replay_paused = true;
                    let clock = host.replay_clock_secs;
                    seek_index(&mut host, clock);
                    let here = host.replay_index;
                    // Between two frames (after a scrub), back steps to the
                    // frame just before the playhead, not the one before it.
                    let i = if h_step < 0 && clock > host.recorded[here].time_secs + 1.0e-4 {
                        here
                    } else {
                        (here as i32 + h_step as i32).clamp(0, host.recorded.len() as i32 - 1) as usize
                    };
                    host.replay_clock_secs = host.recorded[i].time_secs;
                }
                // D-pad up/down, PageUp/PageDown: previous/next marker.
                if v_step != 0 && host.grab.is_none() {
                    let t = host.replay_clock_secs;
                    let marks = marker_times(&host, &mode, last_time);
                    let target = if v_step > 0 {
                        marks.into_iter().filter(|&m| m < t - 1.0e-3).reduce(f32::max)
                    } else {
                        marks.into_iter().filter(|&m| m > t + 1.0e-3).reduce(f32::min)
                    };
                    if let Some(m) = target {
                        host.replay_clock_secs = m;
                        mode.replay_paused = true;
                    }
                }
            }
            // Keyboard X cuts on the press (the pad's X is handled above).
            if keys.just_pressed(KeyCode::KeyX) {
                let t = host.replay_clock_secs;
                add_cut(&mut host, &mut mode, t);
                user_cam_change = true;
                pulse = true;
            }
            // Y / K: keyframe from the view on screen.
            if (pressed & BTN_Y != 0 && !layer && !wheel_active) || keys.just_pressed(KeyCode::KeyK) {
                let t = host.replay_clock_secs;
                capture_keyframe(&mut host, &mut mode, t);
                pulse = true;
            }
            // Ctrl+Z undoes; Ctrl+Y or Ctrl+Shift+Z redoes.
            if ctrl && keys.just_pressed(KeyCode::KeyZ) {
                if shift {
                    redo_edit(&mut host, &mut mode);
                } else {
                    undo_edit(&mut host, &mut mode);
                }
            }
            if ctrl && keys.just_pressed(KeyCode::KeyY) {
                redo_edit(&mut host, &mut mode);
            }
            // P: the TikTok 9:16 render frame.
            if keys.just_pressed(KeyCode::KeyP) {
                mode.portrait = !mode.portrait;
                if mode.portrait {
                    mode.cinematic = false;
                    toast(&mut mode, "TikTok frame on: renders crop to 9:16, 1080x1920");
                } else {
                    toast(&mut mode, "TikTok frame off");
                }
            }
            // 1-9 then 0: cut straight to a camera, one keystroke per shot.
            let digit_cuts: [(KeyCode, u8); 10] = [
                (KeyCode::Digit1, 0),
                (KeyCode::Digit2, 1),
                (KeyCode::Digit3, 2),
                (KeyCode::Digit4, 3),
                (KeyCode::Digit5, 4),
                (KeyCode::Digit6, 5),
                (KeyCode::Digit7, 6),
                (KeyCode::Digit8, 7),
                (KeyCode::Digit9, 8),
                (KeyCode::Digit0, 9),
            ];
            for (key, cam) in digit_cuts {
                if keys.just_pressed(key) {
                    let t = host.replay_clock_secs;
                    mode.replay_cam = cam;
                    cut_to(&mut host, &mut mode, t, cam);
                    user_cam_change = true;
                    pulse = true;
                }
            }
            // Markers under the playhead. B (G on the keyboard): tap to pick
            // one up, so it follows the playhead as you scrub or step, and
            // tap again or press A to put it down. Hold B / Backspace deletes.
            let target = delete_target(&host, host.replay_clock_secs);
            let b_free = !layer && !wheel_active && !host.b_block;
            let grabbing = host.grab.is_some();
            if b_free && buttons & BTN_B != 0 {
                host.b_secs += dt;
            }
            let b_tap = released & BTN_B != 0 && host.b_secs > 0.0 && host.b_secs < 0.3;
            if buttons & BTN_B == 0 {
                host.b_secs = 0.0;
            }
            let key_grab = keys.just_pressed(KeyCode::KeyG);
            if grabbing {
                let a_drop = pressed & BTN_A != 0 && !layer && !wheel_active;
                if b_tap || a_drop || key_grab {
                    if let Some(m) = host.grab.take()
                        && marker_exists(&host, m)
                    {
                        let what = marker_describe(&host, m);
                        let at = marker_time(&host, m);
                        toast(&mut mode, format!("Moved {what} to {at:.2}s"));
                    }
                    pulse = true;
                }
            } else {
                if (b_tap || key_grab)
                    && let Some(m) = target
                {
                    host.grab = Some(m);
                    host.replay_clock_secs = marker_time(&host, m);
                    mode.replay_paused = true;
                    let what = marker_describe(&host, m);
                    let how = if mode.pad_prompts {
                        "LS or D-pad moves it, A or B puts it down"
                    } else {
                        ", and . move it, G puts it down"
                    };
                    toast_for(&mut mode, format!("Moving {what}: {how}"), 3.0);
                    pulse = true;
                }
                if ((b_free && pressed & BTN_B != 0) || keys.just_pressed(KeyCode::Backspace) || key_grab)
                    && target.is_none()
                {
                    let hint = if mode.pad_prompts {
                        "No marker at the playhead. D-pad up / down jumps to one"
                    } else {
                        "No marker at the playhead. PgUp / PgDn jumps to one"
                    };
                    toast(&mut mode, hint);
                }
            }
            let del_down = !grabbing
                && ((b_free && buttons & BTN_B != 0) || keys.pressed(KeyCode::Backspace));
            if host.hold_delete.tick(del_down && target.is_some(), dt, HOLD_DELETE)
                && let Some(m) = target
            {
                let text = delete_marker(&mut host, m);
                toast(&mut mode, text);
                host.b_secs = 0.0;
                pulse = true;
            }
            if let (Some(p), Some(m)) = (host.hold_delete.progress(HOLD_DELETE), target) {
                mode.hold = Some((format!("Hold to delete {}", marker_name(m)), p));
            }
            // Keyboard shortcuts for the editor rows used most.
            if keys.just_pressed(KeyCode::BracketLeft) {
                menu_action(ReplayMenuItem::TrimIn, &mut mode, &mut host);
            }
            if keys.just_pressed(KeyCode::BracketRight) {
                menu_action(ReplayMenuItem::TrimOut, &mut mode, &mut host);
            }
            if keys.just_pressed(KeyCode::Backslash) {
                menu_action(ReplayMenuItem::TrimClear, &mut mode, &mut host);
            }
            if keys.just_pressed(KeyCode::KeyV) {
                menu_action(ReplayMenuItem::RampAdd, &mut mode, &mut host);
            }
            if !free {
                if keys.pressed(KeyCode::Minus) {
                    mode.replay_fov = (cam_fov(&mode) - 30.0 * dt).clamp(10.0, 120.0);
                }
                if keys.pressed(KeyCode::Equal) {
                    mode.replay_fov = (cam_fov(&mode) + 30.0 * dt).clamp(10.0, 120.0);
                }
            }
            if keys.pressed(KeyCode::KeyN) {
                mode.replay_roll = (mode.replay_roll - 0.8 * dt).clamp(-0.6, 0.6);
            }
            if keys.pressed(KeyCode::KeyM) {
                mode.replay_roll = (mode.replay_roll + 0.8 * dt).clamp(-0.6, 0.6);
            }
        }
        if mode.menu_open {
            mode.quick_layer = false;
        }

        // Mouse on the timeline: click or drag the track to scrub, drag the
        // trim lines, drag a marker to move it.
        let mouse = std::mem::take(&mut mode.timeline_mouse);
        if !mode.menu_open {
            let trim_to = |mode: &mut SkateMode, hit: TimelineHit, t: f32| -> f32 {
                if hit == TimelineHit::TrimIn {
                    let hi = if mode.trim_end > 0.0 { mode.trim_end - 0.05 } else { last_time };
                    mode.trim_start = t.clamp(0.0, hi.max(0.0));
                    mode.trim_start
                } else {
                    mode.trim_end = t.clamp(mode.trim_start + 0.05, last_time);
                    mode.trim_end
                }
            };
            for ev in mouse {
                match ev {
                    TimelineMouse::Press(hit, t) => {
                        let t = t.clamp(0.0, last_time);
                        mode.replay_paused = true;
                        host.mouse_drag = Some(hit);
                        let marker = match hit {
                            TimelineHit::Cut(i) => Some(Marker::Cut(i)),
                            TimelineHit::Keyframe(i) => Some(Marker::Keyframe(i)),
                            TimelineHit::Ramp(i) => Some(Marker::Ramp(i)),
                            _ => None,
                        };
                        match (hit, marker) {
                            (_, Some(m)) if marker_exists(&host, m) => {
                                host.grab = Some(m);
                                host.replay_clock_secs = marker_time(&host, m);
                            }
                            (TimelineHit::TrimIn | TimelineHit::TrimOut, _) => {
                                host.replay_clock_secs = trim_to(&mut mode, hit, t);
                            }
                            _ => host.replay_clock_secs = t,
                        }
                    }
                    TimelineMouse::Drag(t) => {
                        let t = t.clamp(0.0, last_time);
                        match host.mouse_drag {
                            Some(hit @ (TimelineHit::TrimIn | TimelineHit::TrimOut)) => {
                                host.replay_clock_secs = trim_to(&mut mode, hit, t);
                            }
                            Some(_) => host.replay_clock_secs = t,
                            None => {}
                        }
                    }
                    TimelineMouse::Release => {
                        if matches!(
                            host.mouse_drag,
                            Some(TimelineHit::Cut(_) | TimelineHit::Keyframe(_) | TimelineHit::Ramp(_))
                        ) {
                            host.grab = None;
                        }
                        host.mouse_drag = None;
                    }
                }
            }
        }

        // Back: tap toggles the help, hold exits the replay. Only a press made
        // during the replay counts, so the one that started it can't.
        if pressed & BTN_BACK != 0 {
            host.back_armed = true;
        }
        if released & BTN_BACK != 0 {
            if host.back_armed && !host.hold_exit.fired && host.hold_exit.secs < 0.35 {
                mode.help_expanded = !mode.help_expanded;
            }
            host.back_armed = false;
        }
        let back_down = buttons & BTN_BACK != 0 && host.back_armed;
        if host.hold_exit.tick(back_down, dt, HOLD_EXIT) {
            mode.replay_requested = true;
            host.back_armed = false;
        }
        if let Some(p) = host.hold_exit.progress(HOLD_EXIT) {
            mode.hold = Some(("Hold to exit replay".into(), p));
        }

        // Console camera requests count as the player switching cameras.
        if let Some(cam) = mode.cut_to_requested.take() {
            if cam < REPLAY_CAM_COUNT {
                let t = host.replay_clock_secs;
                mode.replay_cam = cam;
                cut_to(&mut host, &mut mode, t, cam);
                user_cam_change = true;
            } else {
                toast(&mut mode, format!("Cut: camera {cam} is out of range"));
            }
        }
        // Camera control follows a camera the player picks: on for free,
        // body and board, off for the rest. Playback crossing a cut into one
        // of those leaves it as it was, so a scrub never turns into a flight.
        let cam_now = active_cut(&host.cuts, host.replay_clock_secs).unwrap_or(mode.replay_cam);
        if user_cam_change {
            mode.cam_control = frame::replay_cam_steerable(cam_now);
        } else if !frame::replay_cam_steerable(cam_now) {
            mode.cam_control = false;
        }
        let cam_ctrl = steer && mode.cam_control;

        // Playback, with the triggers as a live slow-mo / fast-forward unless
        // they are moving the camera.
        let mut rate = clip_speed(&host.speed_ramps, mode.replay_speed, host.replay_clock_secs);
        if !cam_ctrl && !mode.menu_open {
            if trig[1] > 0.1 {
                rate *= 1.0 + 3.0 * trig[1];
            }
            if trig[0] > 0.1 {
                rate *= (1.0 - 0.875 * trig[0]).max(0.125);
            }
        }
        if !mode.replay_paused {
            let before = host.replay_clock_secs;
            host.replay_clock_secs += dt * rate;
            // Playing into trim out stops there; past it, at the clip's end.
            let stop_at = if before <= out_time + 1.0e-4 { out_time } else { last_time };
            if host.replay_clock_secs >= stop_at {
                host.replay_clock_secs = stop_at;
                mode.replay_paused = true;
            }
        }

        // Left stick left/right scrubs, quadratic for fine control near the
        // centre. Slow scrubs catch on markers with a short rumble.
        let scrub_free = !(mode.menu_open || layer || wheel_active || (free && cam_ctrl));
        if scrub_free {
            const SCRUB_DEAD: f32 = 0.25;
            let x = lstick[0];
            if x.abs() > SCRUB_DEAD {
                mode.replay_paused = true;
                let m = (x.abs() - SCRUB_DEAD) / (1.0 - SCRUB_DEAD);
                let speed = m * m * SCRUB_MAX * x.signum();
                if host.scrub_sticky > 0.0 {
                    host.scrub_sticky -= dt;
                } else {
                    let from = host.replay_clock_secs;
                    let to = (from + speed * dt).clamp(0.0, last_time);
                    let span = to - from;
                    let snap = if speed.abs() < 1.5 && span != 0.0 {
                        marker_times(&host, &mode, last_time)
                            .into_iter()
                            .filter(|&t| (t - from) * span.signum() > 0.0 && (t - from).abs() <= span.abs())
                            .min_by(|a, b| (a - from).abs().total_cmp(&(b - from).abs()))
                    } else {
                        None
                    };
                    if let Some(t) = snap {
                        host.replay_clock_secs = t;
                        host.scrub_sticky = 0.25;
                        pulse = true;
                    } else {
                        host.replay_clock_secs = to;
                    }
                }
            } else {
                host.scrub_sticky = 0.0;
            }
        }

        // A picked-up marker rides the playhead.
        if let Some(m) = host.grab {
            if marker_exists(&host, m) {
                let t = host.replay_clock_secs;
                if (marker_time(&host, m) - t).abs() > 1.0e-5 {
                    host.grab = Some(move_marker(&mut host, m, t));
                }
            } else {
                host.grab = None;
            }
        }

        if !host.recorded.is_empty() {
            let t_now = host.replay_clock_secs;
            present_at(&mut mode, &mut host, t_now, authority);

            let eff_cam = active_cut(&host.cuts, t_now).unwrap_or(mode.replay_cam);
            // Entering a free-cam or tripod shot (from another camera, or
            // from another such cut) restores that shot's own placement.
            enter_placed_shot(&mut host, &mut mode, t_now, eff_cam, true);
            host.free_edit_secs += dt;
            if eff_cam == 6 && !mode.menu_open && !wheel_active {
                let pad_move = cam_ctrl && !layer;
                // The clamped frame time: a hitch must not fling the camera.
                let moved = update_free_cam(&mut mode, &keys, &input, dt, pad_move);
                // Diagnostic for pads that keep the free cam moving: every
                // quarter second while anything is feeding it, log the raw
                // state of every connected slot and what the camera did.
                host.free_cam_log += dt;
                if host.free_cam_log >= 0.25 {
                    host.free_cam_log = 0.0;
                    let pads = input.connected_sticks();
                    let raw_active = pads.iter().any(|(_, l, r)| {
                        l[0].hypot(l[1]) > 0.08 || r[0].hypot(r[1]) > 0.08
                    }) || trig[0] > 0.05 || trig[1] > 0.05;
                    if raw_active || moved.length_squared() > 1.0e-6 {
                        let held: String = [
                            (KeyCode::KeyW, 'W'),
                            (KeyCode::KeyA, 'A'),
                            (KeyCode::KeyS, 'S'),
                            (KeyCode::KeyD, 'D'),
                            (KeyCode::KeyQ, 'Q'),
                            (KeyCode::KeyE, 'E'),
                        ]
                        .iter()
                        .filter(|(k, _)| keys.pressed(*k))
                        .map(|(_, c)| *c)
                        .collect();
                        let slots: Vec<String> = pads
                            .iter()
                            .map(|(i, l, r)| {
                                format!("{i}:L[{:.3},{:.3}] R[{:.3},{:.3}]", l[0], l[1], r[0], r[1])
                            })
                            .collect();
                        diag::info!(
                            World,
                            "Free cam input: pad={:?} slots=[{}] trig=[{:.2},{:.2}] keys=[{}] move=[{:.2},{:.2},{:.2}]",
                            input.controller(),
                            slots.join(" "),
                            trig[0],
                            trig[1],
                            held,
                            moved.x,
                            moved.y,
                            moved.z
                        );
                    }
                }
            }
            // However the free cam moved this frame (sticks, keys, zoom), the
            // shot on screen is saved into its cut, so playback and the
            // render show exactly this angle.
            if eff_cam == 6 {
                let now_pose = free_pose(&mode);
                if free_slot(&host, t_now) != Some(now_pose) {
                    store_free_slot(&mut host, t_now, now_pose);
                    host.free_edit_secs = 0.0;
                }
            }
            if locked_cam && !mode.menu_open && !wheel_active {
                // Aim the locked mount; the mount itself stays on the body.
                let aim = stick_curve(rstick);
                let mut yaw_d = -aim[0];
                let mut pitch_d = aim[1];
                if keys.pressed(KeyCode::ArrowLeft) { yaw_d += 1.0; }
                if keys.pressed(KeyCode::ArrowRight) { yaw_d -= 1.0; }
                if keys.pressed(KeyCode::ArrowUp) { pitch_d += 1.0; }
                if keys.pressed(KeyCode::ArrowDown) { pitch_d -= 1.0; }
                host.locked_yaw += yaw_d * 1.6 * dt;
                host.locked_pitch = (host.locked_pitch + pitch_d * 1.6 * dt).clamp(-1.2, 1.2);
                // With camera control on, the triggers dolly the mount in and
                // out; the mouse wheel always does.
                let dolly = if cam_ctrl { trig[1] - trig[0] } else { 0.0 };
                host.locked_dist = (host.locked_dist + dolly * 320.0 * dt - wheel_delta * 24.0)
                    .clamp(0.0, 600.0);
            }
            if std::mem::take(&mut mode.keyframe_add_requested) {
                let t = mode.keyframe_add_time.take().unwrap_or(host.replay_clock_secs);
                capture_keyframe(&mut host, &mut mode, t);
            }
            if std::mem::take(&mut mode.cut_add_requested) {
                let t = host.replay_clock_secs;
                add_cut(&mut host, &mut mode, t);
            }

            let heading = replay_heading(&mode);
            apply_replay_camera(&mut mode, host.replay_clock_secs, heading, &mut host, dt);
            host.last_view = mode.camera;

            // Mirrors for the timeline UI, refilled in place every frame.
            mode.keyframe_times.clear();
            mode.keyframe_times.extend(host.keyframes.iter().map(|k| k.time));
            mode.speed_ramps.clear();
            mode.speed_ramps.extend(host.speed_ramps.iter().map(|r| (r.time, r.speed)));
            mode.cut_times.clear();
            mode.cut_times.extend(host.cuts.iter().map(|c| (c.time, c.cam)));
            mode.active_cut_cam =
                active_cut(&host.cuts, host.replay_clock_secs).filter(|c| *c != mode.replay_cam);
            mode.replay_duration = last_time;
            mode.replay_clock = host.replay_clock_secs;
        }

        // Anything that changed the timeline this frame is one undo step. A
        // marker move or trim drag spans many frames and is one step: the
        // state from before it is held until it ends.
        let coalescing = host.grab.is_some()
            || matches!(host.mouse_drag, Some(TimelineHit::TrimIn | TimelineHit::TrimOut))
            || host.free_edit_secs < 0.4;
        if host.history_applied {
            host.drag_base = None;
        } else if coalescing {
            if host.drag_base.is_none() {
                host.drag_base = Some(edits_before);
            }
        } else {
            let base = host.drag_base.take().unwrap_or(edits_before);
            let edits_after = edit_snapshot(&host, &mode);
            if edits_after != base {
                host.undo.push(base);
                if host.undo.len() > UNDO_LIMIT {
                    host.undo.remove(0);
                }
                host.redo.clear();
            }
        }
        mode.grabbed = host
            .grab
            .filter(|m| marker_exists(&host, *m))
            .map(|m| (marker_time(&host, m), marker_describe(&host, m)));
        mode.undo_len = host.undo.len();
        mode.redo_len = host.redo.len();
        mode.eff_cam = active_cut(&host.cuts, host.replay_clock_secs).unwrap_or(mode.replay_cam);
        mode.delete_target = delete_target(&host, host.replay_clock_secs)
            .map(|m| (marker_time(&host, m), marker_describe(&host, m)));
        if mode.menu_open {
            let values: Vec<String> =
                REPLAY_MENU.iter().map(|&item| menu_value(item, &mode, &host)).collect();
            mode.menu_values = values;
        }
        let prompts = replay_prompts(&mode, layer, wheel_active);
        mode.prompts = prompts;

        if pulse && mode.pad_prompts {
            for gamepad in &gamepads {
                rumble.write(GamepadRumbleRequest::Add {
                    gamepad,
                    duration: std::time::Duration::from_millis(70),
                    intensity: GamepadRumbleIntensity {
                        strong_motor: 0.0,
                        weak_motor: 0.45,
                    },
                });
            }
        }
        return;
    }
    if mode.input_blocked {
        if !host.input_suspended
            && let Some(send) = &host.send
        {
            let _ = send.send(Job::Suspend);
        }
        host.input_suspended = true;
        return;
    }
    host.input_suspended = false;
    if let Some(send) = &host.send
        && send
            .send(Job::Step(
                host.epoch,
                time.delta_secs().min(0.1),
                input,
                aspect_ratio,
            ))
            .is_err()
    {
        stop(&mut host, &mut mode, authority);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(t: f32, x: f32) -> RecordedFrame {
        RecordedFrame {
            pose: Pose {
                root: Mat4::from_translation(Vec3::new(x, 0.0, 0.0)),
                bones: vec![Mat4::from_rotation_z(x)],
                names: Vec::new(),
                camera: None,
                velocity: Vec3::ZERO,
                tick: 0,
                state: String::new(),
            },
            time_secs: t,
        }
    }

    #[test]
    fn seek_finds_the_sample_at_or_before() {
        let mut host = Host { recorded: (0..10).map(|i| frame(i as f32 * 0.1, 0.0)).collect(), ..default() };
        seek_index(&mut host, 0.55);
        assert_eq!(host.replay_index, 5);
        seek_index(&mut host, 0.0);
        assert_eq!(host.replay_index, 0);
        seek_index(&mut host, 99.0);
        assert_eq!(host.replay_index, 9);
    }

    #[test]
    fn blend_hits_both_ends_and_the_middle() {
        let a = Mat4::from_rotation_translation(Quat::from_rotation_z(0.0), Vec3::ZERO);
        let b = Mat4::from_rotation_translation(Quat::from_rotation_z(1.0), Vec3::new(2.0, 0.0, 0.0));
        assert!(blend_mat(&a, &b, 0.0).abs_diff_eq(a, 1.0e-5));
        assert!(blend_mat(&a, &b, 1.0).abs_diff_eq(b, 1.0e-5));
        let mid = blend_mat(&a, &b, 0.5);
        let want = Mat4::from_rotation_translation(Quat::from_rotation_z(0.5), Vec3::new(1.0, 0.0, 0.0));
        assert!(mid.abs_diff_eq(want, 1.0e-5));
    }

    /// The render's precomputed frame count must match the frames
    /// `render_step` actually captures: from trim in, while before trim out.
    #[test]
    fn render_frame_count_matches_the_capture_loop() {
        let ramps = vec![SpeedRamp { time: 1.0, speed: 0.25 }, SpeedRamp { time: 2.0, speed: 2.0 }];
        let (in_time, out_time, step) = (0.5f32, 3.0f32, 1.0 / 240.0);
        let mut total = 0u32;
        let mut t = in_time;
        while t < out_time {
            t += step * clip_speed(&ramps, 1.0, t);
            total += 1;
        }
        let mut captured = 0u32;
        let mut clock = in_time;
        let mut first = None;
        while captured < total {
            first.get_or_insert(clock);
            assert!(clock < out_time);
            captured += 1;
            clock = (clock + step * clip_speed(&ramps, 1.0, clock)).min(out_time);
        }
        assert_eq!(first, Some(in_time));
        assert!(clock >= out_time - 1.0e-4);
    }
}

/// A controller's state in XInput's layout, for the skate input.
fn pad_frame(pad: &bevy::input::gamepad::Gamepad, packet: u32) -> InputFrame {
    use bevy::input::gamepad::GamepadButton as B;
    const BITS: [(B, u16); 14] = [
        (B::DPadUp, 0x0001),
        (B::DPadDown, 0x0002),
        (B::DPadLeft, 0x0004),
        (B::DPadRight, 0x0008),
        (B::Start, 0x0010),
        (B::Select, 0x0020),
        (B::LeftThumb, 0x0040),
        (B::RightThumb, 0x0080),
        (B::LeftTrigger, 0x0100),
        (B::RightTrigger, 0x0200),
        (B::South, 0x1000),
        (B::East, 0x2000),
        (B::West, 0x4000),
        (B::North, 0x8000),
    ];
    let buttons = BITS
        .iter()
        .filter(|(button, _)| pad.pressed(*button))
        .fold(0, |bits, (_, bit)| bits | bit);
    // An analog trigger reports its travel; a digital one only pressed.
    let trigger = |button: B| {
        let value = pad
            .get(button)
            .unwrap_or(if pad.pressed(button) { 1.0 } else { 0.0 });
        (value.clamp(0.0, 1.0) * 255.0).round() as u8
    };
    let axis = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16;
    let (left, right) = (pad.left_stick(), pad.right_stick());
    InputFrame::from_pad(
        buttons,
        [trigger(B::LeftTrigger2), trigger(B::RightTrigger2)],
        [axis(left.x), axis(left.y)],
        [axis(right.x), axis(right.y)],
        packet,
    )
}
