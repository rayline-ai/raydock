// Raise the soft RLIMIT_NOFILE at startup.
//
// The macOS default soft limit is 256 file descriptors. The transparent
// proxy holds one upstream and one downstream socket per live Claude
// session connection, so a couple dozen concurrent sessions exhaust the
// default and the daemon dies with EMFILE ("Too many open files").

#[cfg(unix)]
pub const TARGET_NOFILE: libc::rlim_t = 65536;

/// Raise the soft open-file limit to `target`, capped by the hard limit
/// (and by `kern.maxfilesperproc` on macOS, which the kernel enforces even
/// when the hard limit reports infinity). Returns `(before, after)` soft
/// limits; never lowers the limit.
#[cfg(unix)]
pub fn raise_nofile_limit(target: libc::rlim_t) -> std::io::Result<(libc::rlim_t, libc::rlim_t)> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a valid rlimit struct; getrlimit only writes into it.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let before = lim.rlim_cur;

    let mut desired = target.min(lim.rlim_max);
    #[cfg(target_os = "macos")]
    {
        desired = desired.min(macos_maxfilesperproc());
    }
    if desired <= before {
        return Ok((before, before));
    }

    lim.rlim_cur = desired;
    // SAFETY: `lim` is a valid rlimit struct with rlim_cur <= rlim_max.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((before, desired))
}

#[cfg(target_os = "macos")]
fn macos_maxfilesperproc() -> libc::rlim_t {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is NUL-terminated and `value`/`len` describe a valid
    // c_int output buffer; no new value is being set.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&raw mut value).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && value > 0 {
        value as libc::rlim_t
    } else {
        // OPEN_MAX (<sys/syslimits.h>) is the historical per-process
        // ceiling and is always a safe soft-limit value on macOS.
        10240
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn current_soft_limit() -> libc::rlim_t {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `lim` is a valid rlimit struct; getrlimit only writes into it.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
        lim.rlim_cur
    }

    fn set_soft_limit(cur: libc::rlim_t) {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `lim` is a valid rlimit struct; getrlimit only writes into it.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
        lim.rlim_cur = cur;
        // SAFETY: `lim` is a valid rlimit struct with rlim_cur <= rlim_max.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) }, 0);
    }

    // Regression test for the EMFILE crash: with the soft limit at the
    // macOS default of 256, the daemon must raise it well above that.
    #[test]
    fn raises_soft_limit_from_macos_default() {
        let original = current_soft_limit();
        set_soft_limit(256.min(original));

        let (before, after) = raise_nofile_limit(TARGET_NOFILE).expect("raise nofile limit");
        assert_eq!(before, 256.min(original));
        assert!(after >= 4096, "soft limit {after} still near the default");
        assert_eq!(current_soft_limit(), after);

        set_soft_limit(original.max(after));
    }

    #[test]
    fn never_lowers_an_already_high_limit() {
        let original = current_soft_limit();
        let (before, after) = raise_nofile_limit(1).expect("raise nofile limit");
        assert_eq!(before, original);
        assert_eq!(after, original);
        assert_eq!(current_soft_limit(), original);
    }
}
