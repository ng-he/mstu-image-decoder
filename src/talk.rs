//! libav talks to stderr on its own; this takes it to the host's log instead.
//!
//! Its lines arrive from whichever thread is decoding, in C, with no idea of
//! the engine underneath. Without this they land in the terminal unstamped
//! and out of order with everything the engine says.

use std::ffi::CStr;
use std::sync::atomic::{AtomicPtr, Ordering};

use ffmpeg_next::ffi;
use mstu_sdk::{HostContext, LogLevel, Str};

/// Where to send what libav says. One process holds one libav, so the first
/// plugin to come up lends its host and every instance's lines go there.
///
/// Holding the pointer for good is sound because the engine's context is
/// boxed once and outlives every plugin it hands it to, which is the same
/// promise each plugin already relies on.
static HOST: AtomicPtr<HostContext> = AtomicPtr::new(std::ptr::null_mut());

/// The longest line taken from libav; anything past this is cut.
const LINE: usize = 1024;

/// Sends libav's lines to `ctx`'s logger, the first time it is called.
pub fn relay(ctx: *const HostContext) {
    let taken = HOST.compare_exchange(
        std::ptr::null_mut(),
        ctx as *mut HostContext,
        Ordering::AcqRel,
        Ordering::Acquire,
    );

    if taken.is_ok() {
        unsafe {
            ffi::av_log_set_callback(Some(forward));

            // Warnings and worse. Everything below is libav talking to
            // itself, and there is a lot of it.
            ffi::av_log_set_level(ffi::AV_LOG_WARNING);
        }
    }
}

/// What libav calls a level, in the host's words.
fn level_of(level: i32) -> LogLevel {
    match level {
        ffi::AV_LOG_PANIC | ffi::AV_LOG_FATAL | ffi::AV_LOG_ERROR => LogLevel::Error,
        ffi::AV_LOG_WARNING => LogLevel::Warn,
        ffi::AV_LOG_INFO => LogLevel::Info,
        ffi::AV_LOG_VERBOSE | ffi::AV_LOG_DEBUG => LogLevel::Debug,
        _ => LogLevel::Trace,
    }
}

/// Called by libav, on whatever thread was decoding.
///
/// Sound because the host's log hook is a thread safe sink, and the line is
/// formatted by libav itself rather than read as a format string here.
unsafe extern "C" fn forward(
    object: *mut std::ffi::c_void,
    level: i32,
    format: *const std::ffi::c_char,
    arguments: *mut ffi::__va_list_tag,
) {
    let host = HOST.load(Ordering::Acquire);

    if host.is_null() {
        return;
    }

    let mut line = [0 as std::ffi::c_char; LINE];
    let mut prefix = 1;

    let written = unsafe {
        ffi::av_log_format_line2(
            object,
            level,
            format,
            arguments,
            line.as_mut_ptr(),
            LINE as i32,
            &mut prefix,
        )
    };

    if written <= 0 {
        return;
    }

    let text = unsafe { CStr::from_ptr(line.as_ptr()) }.to_string_lossy();
    let text = text.trim_end();

    if text.is_empty() {
        return;
    }

    unsafe {
        ((*host).logger)(level_of(level), Str::from_static("libav"), Str::new(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libav_s_levels_are_said_in_the_host_s_words() {
        assert!(matches!(level_of(ffi::AV_LOG_ERROR), LogLevel::Error));
        assert!(matches!(level_of(ffi::AV_LOG_FATAL), LogLevel::Error));
        assert!(matches!(level_of(ffi::AV_LOG_WARNING), LogLevel::Warn));
        assert!(matches!(level_of(ffi::AV_LOG_INFO), LogLevel::Info));
        assert!(matches!(level_of(ffi::AV_LOG_DEBUG), LogLevel::Debug));
        assert!(matches!(level_of(ffi::AV_LOG_TRACE), LogLevel::Trace));
    }
}
