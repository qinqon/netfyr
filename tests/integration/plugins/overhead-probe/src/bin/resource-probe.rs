//! Small native launcher: unlike forking a large Python supervisor directly,
//! this keeps the child's pre-exec RSS floor below the measured programs' RSS.

use std::io;
use std::mem::MaybeUninit;
use std::process::{Command, Stdio};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let output = args.next().ok_or("pass OUTPUT.json COMMAND [ARG ...]")?;
    let command = args.next().ok_or("pass a command")?;
    let stdout = std::fs::File::create(output)?;
    let start = Instant::now();
    let child = Command::new(command)
        .args(args)
        .stdout(Stdio::from(stdout))
        .spawn()?;
    let mut status = 0;
    let mut usage = MaybeUninit::<libc::rusage>::uninit();
    loop {
        // SAFETY: child.id is a live child PID; status and usage point to
        // correctly sized writable storage filled when wait4 returns that PID.
        let waited = unsafe {
            libc::wait4(
                child.id() as libc::pid_t,
                &mut status,
                0,
                usage.as_mut_ptr(),
            )
        };
        if waited >= 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    // SAFETY: wait4 completed successfully for this child and filled usage.
    let usage = unsafe { usage.assume_init() };
    let milliseconds =
        |time: libc::timeval| time.tv_sec as f64 * 1000.0 + time.tv_usec as f64 / 1000.0;
    let exit_code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    };
    println!(
        "{}",
        serde_json::json!({
            "exit_code":exit_code,"wall_ms":wall_ms,
            "user_ms":milliseconds(usage.ru_utime),"system_ms":milliseconds(usage.ru_stime),
            "peak_rss_kib":usage.ru_maxrss,
            "minor_faults":usage.ru_minflt,"major_faults":usage.ru_majflt,
            "voluntary_context_switches":usage.ru_nvcsw,"involuntary_context_switches":usage.ru_nivcsw,
        })
    );
    if exit_code != 0 {
        std::process::exit(1);
    }
    Ok(())
}
