//! On-screen messages after a respawn ("Delayed respawn avoided"), in the
//! style of the game's own banners such as "The Night's tide grows stronger".
//!
//! ## How the game shows those banners
//!
//! The banners are "text effects" (`FeTextEffectParam` rows). The Night's Tide
//! one is row 4200 (text `CL_MenuText` 423100). When the day phase changes,
//! the Night's Tide code calls a small helper that does
//! `show_text_effect(hud + 0xebd0, 4200)`. A second variant,
//! `show_text_effect_with_text(hud + 0xebd0, row, text)`, also takes a text
//! object. When the banner appears, a non-empty text object wins over the
//! row's own text, so the DLL shows row 4200 (the Night's Tide look and
//! sound) with its own text. Nothing shared is changed: the game's own
//! banners pass no text and keep theirs.
//!
//! The request goes into a queue that the HUD empties every frame, so it must
//! be made from the game's own thread. That is why the DLL hooks the call to
//! the HUD's per-frame update (the same function that calls the second
//! variant itself): the hook calls the original update, then shows a pending
//! message, if any (`hook.rs`).
//!
//! ## The hook
//!
//! The call site is a 5-byte `call rel32`. The DLL only changes its 4-byte
//! target (one store, so the game thread sees either the old or the new
//! target), to a small read-execute jump stub allocated near the game's code
//! that leads to `hook::hud_update`. The original target is restored when the
//! `Banner` is dropped.
//!
//! The hook is installed only when the first message is about to be shown
//! (never with `notify = false`), and it switches itself off after a panic or
//! a text-object layout mismatch. Its outcomes come back through
//! `take_status`, so the log says when a message was not shown.

mod hook;

#[cfg(feature = "dev")]
pub use hook::rearm;
pub use hook::take_status;

use crate::mem::{read, rel32_target, write_code_u32};
use crate::scan::{Pattern, find_unique, find_unique_or_same, has_bytes, pattern};

/// The call to the HUD's per-frame update (`call rel32` at offset 6).
///
/// ```text
/// 0F 28 CE                    movaps xmm1, xmm6             ; frame time
/// 48 8B CB                    mov    rcx, rbx               ; HUD
/// E8 ?? ?? ?? ??              call   HUD::update_text_effects  <- hooked
/// 83 BB ?? ?? ?? ?? 00        cmp    dword [rbx + ...], 0
/// 7E ??                       jle    ...
/// 80 BB ?? ?? ?? ?? 00        cmp    byte [rbx + ...], 0
/// 75 ??                       jne    ...
/// C7 83                       mov    dword [rbx + ...], ...
/// ```
const UPDATE_CALL: &Pattern = pattern!(
    "0F 28 CE 48 8B CB E8 ?? ?? ?? ?? 83 BB ?? ?? ?? ?? 00 7E ?? 80 BB ?? ?? ?? ?? 00 75 ?? C7 83"
);
const UPDATE_CALL_OFFSET: usize = 6;

/// The Night's Tide banner helper: gives the text effect manager's offset in
/// the HUD (at 13) and `show_text_effect` (jump at 17).
///
/// ```text
/// 80 FA 01                    cmp  dl, 1
/// 75 ??                       jne  ...
/// BA 68 10 00 00              mov  edx, 4200                ; "The Night's tide grows stronger"
/// 48 81 C1 ?? ?? ?? ??        add  rcx, <text effect manager>
/// E9 ?? ?? ?? ??              jmp  show_text_effect
/// 80 FA 02                    cmp  dl, 2
/// 75 ??                       jne  ...
/// BA 69 10 00 00              mov  edx, 4201                ; "Rains gather, ..."
/// ```
const TIDE_BANNER: &Pattern = pattern!(
    "80 FA 01 75 ?? BA 68 10 00 00 48 81 C1 ?? ?? ?? ?? E9 ?? ?? ?? ?? 80 FA 02 75 ?? BA 69 10 00 00"
);
const TIDE_MANAGER_OFFSET: usize = 13;
const TIDE_SHOW_JUMP: usize = 17;

/// The HUD update's own call of `show_text_effect_with_text` (call at 15).
///
/// ```text
/// 48 8D 8B ?? ?? ?? ??        lea  rcx, [rbx + <text effect manager>]
/// 4C 8B C0                    mov  r8, rax                  ; text
/// BA 3E 08 00 00              mov  edx, 2110
/// E8 ?? ?? ?? ??              call show_text_effect_with_text
/// ```
const SHOW_WITH_TEXT_CALL: &Pattern =
    pattern!("48 8D 8B ?? ?? ?? ?? 4C 8B C0 BA 3E 08 00 00 E8 ?? ?? ?? ??");
const SHOW_WITH_TEXT_CALL_OFFSET: usize = 15;

/// Inside `show_text_effect`: the constructor of the text object (call at 13).
/// The pattern matches twice (two variants); both call the same constructor.
///
/// ```text
/// 8B 44 24 28                 mov  eax, [rsp + 0x28]
/// 89 44 24 40                 mov  [rsp + 0x40], eax
/// 48 8D 4C 24 48              lea  rcx, [rsp + 0x48]
/// E8 ?? ?? ?? ??              call TextArg::new
/// ```
const TEXT_CTOR_CALL: &Pattern = pattern!("8B 44 24 28 89 44 24 40 48 8D 4C 24 48 E8 ?? ?? ?? ??");
const TEXT_CTOR_CALL_OFFSET: usize = 13;

// The offsets above point at the opcode or field they name.
const _: () = assert!(has_bytes(UPDATE_CALL, UPDATE_CALL_OFFSET, &[0xE8]));
const _: () = assert!(has_bytes(
    TIDE_BANNER,
    TIDE_MANAGER_OFFSET - 3,
    &[0x48, 0x81, 0xC1]
));
const _: () = assert!(has_bytes(TIDE_BANNER, TIDE_SHOW_JUMP, &[0xE9]));
const _: () = assert!(has_bytes(
    SHOW_WITH_TEXT_CALL,
    SHOW_WITH_TEXT_CALL_OFFSET,
    &[0xE8]
));
const _: () = assert!(has_bytes(TEXT_CTOR_CALL, TEXT_CTOR_CALL_OFFSET, &[0xE8]));

/// `FeTextEffectParam` row of the Night's Tide banner.
pub const NIGHT_TIDE_EFFECT: u32 = 4200;

pub const AVOIDED: &str = "Delayed respawn avoided";
pub const NOT_AVOIDED: &str = "Delayed respawn couldn't be avoided. Check mod logs";

/// Which message to show.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Message {
    Avoided,
    NotAvoided,
}

impl Message {
    pub fn text(self) -> &'static str {
        match self {
            Message::Avoided => AVOIDED,
            Message::NotAvoided => NOT_AVOIDED,
        }
    }

    fn kind(self) -> u64 {
        match self {
            Message::Avoided => hook::KIND_AVOIDED,
            Message::NotAvoided => hook::KIND_NOT_AVOIDED,
        }
    }
}

/// The installed hook. Dropping it puts the original call back.
pub struct Banner {
    /// Address of the `rel32` in the hooked call.
    rel_addr: usize,
    original_rel: u32,
    hooked_rel: u32,
}

impl Banner {
    /// Finds the game functions and hooks the HUD update call.
    pub fn install() -> Result<Self, String> {
        let call = find_unique(UPDATE_CALL, "HUD update call")? + UPDATE_CALL_OFFSET;
        let tide = find_unique(TIDE_BANNER, "Night's Tide banner")?;
        let with_text = find_unique(SHOW_WITH_TEXT_CALL, "text banner call")?;
        let ctor = find_unique_or_same(TEXT_CTOR_CALL, TEXT_CTOR_CALL_OFFSET, "text constructor")?;

        let targets = hook::Targets {
            manager_offset: read::<u32>(tide + TIDE_MANAGER_OFFSET)
                .ok_or("cannot read the banner manager offset")?
                as usize,
            show: rel32_target(tide + TIDE_SHOW_JUMP).ok_or("cannot read show_text_effect")?,
            show_with_text: rel32_target(with_text + SHOW_WITH_TEXT_CALL_OFFSET)
                .ok_or("cannot read the text variant")?,
            text_ctor: ctor,
            original_update: rel32_target(call).ok_or("cannot read the HUD update")?,
        };
        let original_rel = read::<u32>(call + 1).ok_or("cannot read the HUD update call")?;
        hook::init(&targets, AVOIDED, NOT_AVOIDED);

        let stub = hook::make_stub(call).ok_or("no memory near the game's code for the hook")?;
        let rel = stub as i64 - (call as i64 + 5);
        let hooked_rel = i32::try_from(rel).map_err(|_| "hook stub out of rel32 range")? as u32;
        if !write_code_u32(call + 1, original_rel, hooked_rel) {
            return Err("could not hook the HUD update call".into());
        }
        Ok(Self {
            rel_addr: call + 1,
            original_rel,
            hooked_rel,
        })
    }

    /// Asks the game thread to show a message on its next HUD update.
    pub fn show(&self, message: Message) {
        hook::request(message.kind(), NIGHT_TIDE_EFFECT);
    }

    /// Test only (dev builds): show any text effect row, with its own text or
    /// ours.
    #[cfg(feature = "dev")]
    pub fn show_raw(&self, row: u32, text: Option<Message>) {
        hook::request(text.map_or(hook::KIND_PLAIN, Message::kind), row);
    }
}

impl Drop for Banner {
    fn drop(&mut self) {
        write_code_u32(self.rel_addr, self.hooked_rel, self.original_rel);
    }
}
