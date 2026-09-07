//! Optional priority for musical timing threads.

pub(crate) fn set_priority(priority: u8) -> Result<(), String> {
    if priority == 0 {
        return Ok(());
    }
    if priority > 99 {
        return Err("RT priority must be 0 (disabled) or 1–99".into());
    }
    #[cfg(target_os = "linux")]
    {
        let param = libc::sched_param { sched_priority: i32::from(priority) };
        // pid 0 selects the calling thread; other workers keep their policy.
        if unsafe { libc::sched_setscheduler(0, libc::SCHED_RR, &param) } != 0 {
            return Err(format!("cannot set RT priority {priority}: {}; grant an rtprio limit or omit --rt-priority", std::io::Error::last_os_error()));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    Err("RT priority is supported on Linux only; omit --rt-priority".into())
}
