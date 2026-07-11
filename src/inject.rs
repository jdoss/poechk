//! Injects the item-copy chord into the game via XTest.
//!
//! PoE runs under XWayland (Proton), so faking Ctrl+Alt+C through the X server
//! delivers real key events resolved against the game's actual keymap — the
//! same mechanism Awakened PoE Trade uses. This avoids the Wayland
//! virtual-keyboard path, whose synthetic keymap XWayland misinterprets.

use std::time::Duration;

use anyhow::Context;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, KEY_PRESS_EVENT, KEY_RELEASE_EVENT};
use x11rb::protocol::xtest::ConnectionExt as _;

const XK_CONTROL_L: u32 = 0xffe3;
const XK_ALT_L: u32 = 0xffe9;
const XK_LOWER_C: u32 = 0x0063;

/// Small gap between faked events so Wine/Proton reads a clean chord.
const KEY_GAP: Duration = Duration::from_millis(10);

/// Fake Ctrl+Alt+C on the X (XWayland) server the game is connected to.
pub fn send_copy_chord() -> anyhow::Result<()> {
    let (conn, _screen) =
        x11rb::connect(None).context("connecting to XWayland — is DISPLAY set?")?;

    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let mapping = conn
        .get_keyboard_mapping(min, max - min + 1)?
        .reply()
        .context("reading the X keyboard mapping")?;
    let keycode_for = |keysym: u32| -> anyhow::Result<u8> {
        let per = mapping.keysyms_per_keycode as usize;
        mapping
            .keysyms
            .chunks(per)
            .position(|syms| syms.contains(&keysym))
            .map(|idx| min + idx as u8)
            .with_context(|| format!("keysym {keysym:#x} not present in the X keymap"))
    };
    let ctrl = keycode_for(XK_CONTROL_L)?;
    let alt = keycode_for(XK_ALT_L)?;
    let c = keycode_for(XK_LOWER_C)?;

    let chord = [
        (KEY_PRESS_EVENT, ctrl),
        (KEY_PRESS_EVENT, alt),
        (KEY_PRESS_EVENT, c),
        (KEY_RELEASE_EVENT, c),
        (KEY_RELEASE_EVENT, alt),
        (KEY_RELEASE_EVENT, ctrl),
    ];
    for (kind, keycode) in chord {
        conn.xtest_fake_input(kind, keycode, x11rb::CURRENT_TIME, 0, 0, 0, 0)?
            .check()
            .context("sending a faked key event")?;
        std::thread::sleep(KEY_GAP);
    }
    conn.flush().context("flushing the X connection")?;
    Ok(())
}
