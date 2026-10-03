# Skate Replay Mod for IW4L-Skate

A replay editor for Skate mode. It has a playback HUD, a timeline you can scrub
and click, play / pause and frame step, 14 cameras with cuts and keyframes,
speed ramps, trimming, save / load, and rendering to MP4. It also lets you
change the skater's skin.

Made for **IW4L-Skate v0.4.0** (the current `main` on
https://github.com/chasmlol/iw4l-skate).

## Install

1. Get the IW4L-Skate source code (v0.4.0):
   `git clone https://github.com/chasmlol/iw4l-skate`
2. Copy the `crates` and `skate` folders from this mod into the `iw4l-skate`
   folder. When Windows asks, choose **Replace the files in the destination**.
   (Only the files listed below are replaced or added.)
3. Build the game:
   `cargo build --release -p launcher`
   The new `iw4l.exe` is in `target\release\`. Copy it over the `iw4l.exe` in
   your IW4L-Skate game folder.

If you use git, you can apply `skate-replay.patch` instead of copying:
`git am skate-replay.patch`

### Files

| File | |
| --- | --- |
| `crates/console/src/debug_move.rs` | replay HUD: timeline, camera strip, editor menu, prompts, load list |
| `crates/console/src/replay_icons.rs` | **new**: button / camera icons |
| `crates/console/src/plugin/mod.rs` | registers the replay UI, C / slash / U / I keys, replay font |
| `crates/console/src/gamepad.rs` | Start opens the replay menu, not the pause menu, during a replay |
| `crates/console/src/lib.rs` | adds `replay_icons` |
| `crates/frame/src/skate.rs`, `crates/frame/src/lib.rs` | replay state, menu rows, camera names |
| `crates/render_anim/src/skate.rs` | recording, playback, cameras, editing, save / load, MP4 render |
| `crates/render_anim/Cargo.toml` | adds `flate2` and JPEG support |
| `crates/assets/src/bot_model.rs` | skater skins |
| `crates/render_anim/src/skate/rig.rs`, `occupancy/remote_body.rs`, `anim/model_materials.rs` | draw the chosen skin, and draw the board on every map |
| `skate/crates/skate-host/src/physics/bridge.rs` | stick and trigger values for the replay controls |

## Needs

- **Render video** needs `ffmpeg` on your PATH (https://ffmpeg.org).
- **Skins** are optional: put skin `.json` models in `skate-data\assets\skins\`.
  Without any, the skin option shows only the default.
- Replays you save go to `iw4l-artifacts\skate-demos\`.

## Using it

While skating (controller):
- **Back**: start / stop recording
- **LB + Back**: play the recording
- **RB + Back**: instant replay of the last 45 seconds
- **U / I** (keyboard): change skin

Console (`~`): `skate record`, `skate replay`, `skate cam <n|name>`,
`skate speed <0.125..4>`, `skate fov <10..120>`, `skate keyframe`, `skate cut`,
`skate ease <smooth|linear>`.

### In a replay, controller

```
PLAYBACK
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

Start  editor menu   LS click  hide HUD   Hold Back  exit
```

### In a replay, keyboard and mouse

```
PLAYBACK
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
[ ]  trim in / out         \  clear trim
V  speed ramp              - =  zoom    N M  roll
P  TikTok 9:16 frame       Ctrl+Z / Ctrl+Y  undo / redo
```

Press `/` in a replay to show the full controls on screen. Tab opens the editor menu.

## Licence

Apache-2.0, same as IW4L. Unofficial fan mod, not affiliated with Activision,
Infinity Ward or EA. No game data included.
