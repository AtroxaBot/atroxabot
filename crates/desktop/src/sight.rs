//! Whether anyone can see the window while it does not have the keyboard, so that what nobody sees is not
//! drawn. macOS and Wayland say when a window is covered (`WindowEvent::Occluded`); Windows does not, and
//! goes on composing a window that lies under others, so there the windows above it are measured here.

use winit::window::Window;

/// What can be seen of a window that does not have the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sight {
    /// Nothing: other windows lie over all of it, or it is on another desktop.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code, reason = "only Windows measures what lies over a window"))]
    Covered,
    /// Some of it, with the mouse elsewhere.
    Open,
    /// The mouse is over it.
    Pointed,
}

/// A rectangle of the screen in pixels, laid out as Win32's `RECT`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code, reason = "only Windows has to measure"))]
struct Bounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code, reason = "only Windows has to measure"))]
impl Bounds {
    fn is_empty(self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    /// What `self` and `other` share; empty when they share nothing.
    fn clip(self, other: Self) -> Self {
        Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        }
    }
}

/// Takes `cover` out of `seen`, the pieces of a window that can still be seen: a piece it lies over is cut
/// into the strips that stick out around it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code, reason = "only Windows has to measure"))]
fn cover(seen: &mut Vec<Bounds>, cover: Bounds) {
    let mut left = Vec::with_capacity(seen.len());
    for piece in seen.drain(..) {
        let cut = piece.clip(cover);
        if cut.is_empty() {
            left.push(piece);
            continue;
        }
        let strips = [
            Bounds { bottom: cut.top, ..piece },
            Bounds { top: cut.bottom, ..piece },
            Bounds { top: cut.top, bottom: cut.bottom, right: cut.left, ..piece },
            Bounds { top: cut.top, bottom: cut.bottom, left: cut.right, ..piece },
        ];
        left.extend(strips.into_iter().filter(|strip| !strip.is_empty()));
    }
    *seen = left;
}

/// What can be seen of `window`. `pointed` is whether the mouse was last seen over it, which is all there is
/// to go by where the platform itself says when a window is covered.
#[must_use]
pub fn look(window: &Window, pointed: bool) -> Sight {
    #[cfg(target_os = "windows")]
    {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        // Windows is asked where the mouse is: a window that slides over this one may not say it left.
        let _ = pointed;
        match window.window_handle().map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Win32(handle)) => win::look(handle.hwnd.get() as win::Hwnd),
            _ => Sight::Open,
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = window;
        if pointed { Sight::Pointed } else { Sight::Open }
    }
}

#[cfg(target_os = "windows")]
mod win {
    use std::ffi::c_void;

    use super::{Bounds, Sight, cover};

    pub type Hwnd = *mut c_void;

    /// Win32's `POINT`.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetCursorPos(point: *mut Point) -> i32;
        fn WindowFromPoint(point: Point) -> Hwnd;
        fn GetAncestor(window: Hwnd, which: u32) -> Hwnd;
        fn GetWindow(window: Hwnd, which: u32) -> Hwnd;
        fn IsWindowVisible(window: Hwnd) -> i32;
        fn IsIconic(window: Hwnd) -> i32;
        fn GetWindowLongW(window: Hwnd, index: i32) -> i32;
        fn GetLayeredWindowAttributes(window: Hwnd, key: *mut u32, alpha: *mut u8, flags: *mut u32) -> i32;
        fn GetWindowRect(window: Hwnd, bounds: *mut Bounds) -> i32;
        fn GetWindowRgnBox(window: Hwnd, bounds: *mut Bounds) -> i32;
        fn GetSystemMetrics(index: i32) -> i32;
    }

    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmGetWindowAttribute(window: Hwnd, attribute: u32, value: *mut c_void, size: u32) -> i32;
    }

    /// `GA_ROOT`: the top-level window a window belongs to.
    const ROOT: u32 = 2;
    /// `GW_HWNDPREV`: the window just above in the stacking order.
    const ABOVE: u32 = 3;
    /// `GWL_EXSTYLE`.
    const EX_STYLE: i32 = -20;
    /// `WS_EX_TRANSPARENT`: the window is drawn after those under it, which show through.
    const EX_TRANSPARENT: u32 = 0x20;
    /// `WS_EX_LAYERED`: the window may be see-through.
    const EX_LAYERED: u32 = 0x8_0000;
    /// `LWA_COLORKEY`: one colour of a layered window is see-through.
    const COLOR_KEY: u32 = 1;
    /// `LWA_ALPHA`: a layered window is see-through as a whole.
    const ALPHA: u32 = 2;
    /// `DWMWA_EXTENDED_FRAME_BOUNDS`: a window's frame as drawn, without the unseen border it is resized by.
    const FRAME_BOUNDS: u32 = 9;
    /// `DWMWA_CLOAKED`: the window is not shown though it counts as visible (another desktop, a suspended app).
    const CLOAKED: u32 = 14;
    /// `SM_XVIRTUALSCREEN` and the three after it: the box around all screens.
    const SCREENS: [i32; 4] = [76, 77, 78, 79];
    /// The windows above looked at, at most: the order may change while it is walked.
    const MAX_WINDOWS: usize = 2048;
    /// The pieces a window is cut into, at most; past that it is taken to be seen.
    const MAX_PIECES: usize = 256;

    /// Whether Windows keeps `window` off the screen though it counts as visible.
    fn cloaked(window: Hwnd) -> bool {
        let mut cloaked: u32 = 0;
        // SAFETY: this attribute is one DWORD, written through a pointer to one, with its size.
        let read = unsafe { DwmGetWindowAttribute(window, CLOAKED, (&raw mut cloaked).cast(), 4) };
        read == 0 && cloaked != 0
    }

    /// Where `window` is drawn on the screen.
    fn frame(window: Hwnd) -> Option<Bounds> {
        let mut bounds = Bounds::default();
        // SAFETY: this attribute is one RECT, which `Bounds` is laid out as, with its size; `GetWindowRect`
        // writes one RECT too.
        let read = unsafe {
            DwmGetWindowAttribute(window, FRAME_BOUNDS, (&raw mut bounds).cast(), 16) == 0
                || GetWindowRect(window, &raw mut bounds) != 0
        };
        (read && !bounds.is_empty()).then_some(bounds)
    }

    /// The part of the screen `window` hides what is under it in: none for a window that is not shown, lets
    /// what is under it show through, or is not a plain rectangle.
    fn hides(window: Hwnd) -> Option<Bounds> {
        // SAFETY: these only read the state of a window handle, which may name a window that is gone: they
        // then answer zero. The pointers are to live values of the types the calls write.
        unsafe {
            if IsWindowVisible(window) == 0 || IsIconic(window) != 0 {
                return None;
            }
            let style = GetWindowLongW(window, EX_STYLE).cast_unsigned();
            if style & EX_TRANSPARENT != 0 {
                return None;
            }
            if style & EX_LAYERED != 0 {
                let (mut key, mut alpha, mut flags) = (0u32, 0u8, 0u32);
                // Refused for a layered window that is drawn with see-through pixels of its own.
                let read = GetLayeredWindowAttributes(window, &raw mut key, &raw mut alpha, &raw mut flags) != 0;
                if !read || flags & COLOR_KEY != 0 || (flags & ALPHA != 0 && alpha < u8::MAX) {
                    return None;
                }
            }
            // A window cut to a shape of its own covers less than its frame.
            let mut shape = Bounds::default();
            if GetWindowRgnBox(window, &raw mut shape) != 0 {
                return None;
            }
        }
        if cloaked(window) { None } else { frame(window) }
    }

    /// What can be seen of the top-level window `window`.
    pub fn look(window: Hwnd) -> Sight {
        let mut point = Point::default();
        // SAFETY: `GetCursorPos` writes one POINT; the others take the handles they were just given.
        let pointed = unsafe {
            GetCursorPos(&raw mut point) != 0 && {
                let under = WindowFromPoint(point);
                !under.is_null() && GetAncestor(under, ROOT) == window
            }
        };
        if pointed {
            return Sight::Pointed;
        }
        if cloaked(window) {
            return Sight::Covered;
        }
        let Some(own) = frame(window) else { return Sight::Open };
        // SAFETY: `GetSystemMetrics` takes any index.
        let [x, y, width, height] = SCREENS.map(|index| unsafe { GetSystemMetrics(index) });
        let screens = Bounds { left: x, top: y, right: x.saturating_add(width), bottom: y.saturating_add(height) };
        let mut seen = vec![own.clip(screens)];
        seen.retain(|piece| !piece.is_empty());
        // SAFETY: `GetWindow` takes any handle and answers null for one that is gone or has none above.
        let mut above = unsafe { GetWindow(window, ABOVE) };
        for _ in 0..MAX_WINDOWS {
            if seen.is_empty() || above.is_null() {
                break;
            }
            if let Some(hidden) = hides(above) {
                cover(&mut seen, hidden);
                if seen.len() > MAX_PIECES {
                    return Sight::Open;
                }
            }
            // SAFETY: as above.
            above = unsafe { GetWindow(above, ABOVE) };
        }
        if seen.is_empty() { Sight::Covered } else { Sight::Open }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn bounds(left: i32, top: i32, right: i32, bottom: i32) -> Bounds {
        Bounds { left, top, right, bottom }
    }

    fn area(pieces: &[Bounds]) -> i64 {
        pieces.iter().map(|piece| i64::from(piece.right - piece.left) * i64::from(piece.bottom - piece.top)).sum()
    }

    #[test]
    fn a_window_over_all_of_it_leaves_nothing_to_see() {
        let mut seen = vec![bounds(100, 100, 900, 700)];
        cover(&mut seen, bounds(0, 0, 1920, 1040));
        assert!(seen.is_empty());
        // The same size exactly, as two maximised windows are.
        let mut seen = vec![bounds(0, 0, 1920, 1040)];
        cover(&mut seen, bounds(0, 0, 1920, 1040));
        assert!(seen.is_empty());
    }

    #[test]
    fn a_window_over_part_of_it_leaves_the_rest() {
        let mut seen = vec![bounds(0, 0, 100, 100)];
        cover(&mut seen, bounds(25, 25, 75, 75));
        // The middle is gone: a strip above, one below and one at either side are left, none overlapping.
        assert_eq!(seen.len(), 4);
        assert_eq!(area(&seen), 100 * 100 - 50 * 50);
        cover(&mut seen, bounds(200, 200, 300, 300));
        assert_eq!(area(&seen), 100 * 100 - 50 * 50, "a window beside it covers nothing");
        cover(&mut seen, bounds(-10, -10, 50, 200));
        assert_eq!(area(&seen), 50 * 100 - 25 * 50);
        // Two windows side by side cover what neither does alone.
        cover(&mut seen, bounds(50, 0, 100, 100));
        assert!(seen.is_empty());
    }

    #[test]
    fn empty_and_inside_out_rectangles_cover_nothing() {
        let mut seen = vec![bounds(0, 0, 10, 10)];
        cover(&mut seen, bounds(5, 5, 5, 9));
        cover(&mut seen, bounds(8, 8, 2, 2));
        assert_eq!(seen, [bounds(0, 0, 10, 10)]);
        assert!(bounds(3, 3, 3, 4).is_empty() && !bounds(0, 0, 1, 1).is_empty());
    }
}
