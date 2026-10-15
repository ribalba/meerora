use meerora_term::{Key, Mods, encode_key};

const NONE: Mods = Mods { ctrl: false, alt: false, shift: false };
const CTRL: Mods = Mods { ctrl: true, alt: false, shift: false };
const ALT: Mods = Mods { ctrl: false, alt: true, shift: false };
const SHIFT: Mods = Mods { ctrl: false, alt: false, shift: true };

fn enc(key: Key, mods: Mods) -> Vec<u8> {
    encode_key(key, mods, false).expect("key should send bytes")
}

#[test]
fn plain_keys() {
    assert_eq!(enc(Key::Enter, NONE), b"\r");
    assert_eq!(enc(Key::Backspace, NONE), b"\x7f");
    assert_eq!(enc(Key::Tab, NONE), b"\t");
    assert_eq!(enc(Key::Escape, NONE), b"\x1b");
    assert_eq!(enc(Key::Insert, NONE), b"\x1b[2~");
    assert_eq!(enc(Key::Delete, NONE), b"\x1b[3~");
    assert_eq!(enc(Key::PageUp, NONE), b"\x1b[5~");
    assert_eq!(enc(Key::PageDown, NONE), b"\x1b[6~");
}

#[test]
fn shift_tab_is_backtab() {
    assert_eq!(enc(Key::Tab, SHIFT), b"\x1b[Z");
}

#[test]
fn cursor_keys_follow_decckm() {
    let normal: Vec<_> = [Key::Up, Key::Down, Key::Right, Key::Left, Key::Home, Key::End]
        .into_iter()
        .map(|key| encode_key(key, NONE, false).unwrap())
        .collect();
    assert_eq!(normal, [b"\x1b[A", b"\x1b[B", b"\x1b[C", b"\x1b[D", b"\x1b[H", b"\x1b[F"]);

    let app: Vec<_> = [Key::Up, Key::Down, Key::Right, Key::Left, Key::Home, Key::End]
        .into_iter()
        .map(|key| encode_key(key, NONE, true).unwrap())
        .collect();
    assert_eq!(app, [b"\x1bOA", b"\x1bOB", b"\x1bOC", b"\x1bOD", b"\x1bOH", b"\x1bOF"]);
}

#[test]
fn modifier_parameters() {
    assert_eq!(enc(Key::Up, CTRL), b"\x1b[1;5A");
    assert_eq!(enc(Key::Left, SHIFT), b"\x1b[1;2D");
    assert_eq!(enc(Key::Right, ALT), b"\x1b[1;3C");
    assert_eq!(enc(Key::End, Mods { ctrl: true, alt: false, shift: true }), b"\x1b[1;6F");
    assert_eq!(enc(Key::Home, Mods { ctrl: true, alt: true, shift: true }), b"\x1b[1;8H");
    // Modified cursor keys use CSI even in application cursor mode.
    assert_eq!(encode_key(Key::Up, CTRL, true).unwrap(), b"\x1b[1;5A");
    assert_eq!(enc(Key::Delete, CTRL), b"\x1b[3;5~");
    assert_eq!(enc(Key::PageUp, SHIFT), b"\x1b[5;2~");
}

#[test]
fn function_keys() {
    let expected: [&[u8]; 12] = [
        b"\x1bOP",
        b"\x1bOQ",
        b"\x1bOR",
        b"\x1bOS",
        b"\x1b[15~",
        b"\x1b[17~",
        b"\x1b[18~",
        b"\x1b[19~",
        b"\x1b[20~",
        b"\x1b[21~",
        b"\x1b[23~",
        b"\x1b[24~",
    ];
    for (n, want) in (1..=12).zip(expected) {
        assert_eq!(enc(Key::F(n), NONE), want, "F{n}");
    }
    assert_eq!(enc(Key::F(1), SHIFT), b"\x1b[1;2P");
    assert_eq!(enc(Key::F(4), CTRL), b"\x1b[1;5S");
    assert_eq!(enc(Key::F(5), CTRL), b"\x1b[15;5~");
    assert_eq!(enc(Key::F(12), ALT), b"\x1b[24;3~");
    assert_eq!(encode_key(Key::F(0), NONE, false), None);
    assert_eq!(encode_key(Key::F(13), NONE, false), None);
}

#[test]
fn ctrl_letters_give_control_codes() {
    assert_eq!(enc(Key::Char('a'), CTRL), [0x01]);
    assert_eq!(enc(Key::Char('c'), CTRL), [0x03]);
    assert_eq!(enc(Key::Char('z'), CTRL), [0x1a]);
    assert_eq!(enc(Key::Char('C'), Mods { ctrl: true, alt: false, shift: true }), [0x03]);
    assert_eq!(enc(Key::Char(' '), CTRL), [0x00]);
    assert_eq!(enc(Key::Char('@'), CTRL), [0x00]);
    assert_eq!(enc(Key::Char('['), CTRL), [0x1b]);
    assert_eq!(enc(Key::Char('\\'), CTRL), [0x1c]);
    assert_eq!(enc(Key::Char(']'), CTRL), [0x1d]);
    assert_eq!(enc(Key::Char('^'), CTRL), [0x1e]);
    assert_eq!(enc(Key::Char('_'), CTRL), [0x1f]);
    assert_eq!(enc(Key::Char('/'), CTRL), [0x1f]);
    assert_eq!(enc(Key::Char('?'), CTRL), [0x7f]);
    // Characters without a control code are sent unchanged.
    assert_eq!(enc(Key::Char('.'), CTRL), b".");
    assert_eq!(enc(Key::Char('ä'), CTRL), "ä".as_bytes());
}

#[test]
fn alt_prefixes_escape() {
    assert_eq!(enc(Key::Char('x'), ALT), b"\x1bx");
    assert_eq!(enc(Key::Char('ß'), ALT), "\x1bß".as_bytes());
    assert_eq!(enc(Key::Char('b'), Mods { ctrl: true, alt: true, shift: false }), [0x1b, 0x02]);
    assert_eq!(enc(Key::Backspace, ALT), b"\x1b\x7f");
    assert_eq!(enc(Key::Enter, ALT), b"\x1b\r");
}

#[test]
fn plain_characters_pass_through() {
    assert_eq!(enc(Key::Char('q'), NONE), b"q");
    assert_eq!(enc(Key::Char('Q'), SHIFT), b"Q");
    assert_eq!(enc(Key::Char('€'), NONE), "€".as_bytes());
}

#[test]
fn ctrl_backspace_sends_backspace_code() {
    assert_eq!(enc(Key::Backspace, CTRL), [0x08]);
}
