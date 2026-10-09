use std::io;

#[cfg(unix)]
pub(crate) fn configure_read() -> io::Result<()> {
    use rustix::termios::{OptionalActions, SpecialCodeIndex, tcgetattr, tcsetattr};
    use std::io::IsTerminal;
    use std::os::fd::AsFd;

    let stdin = io::stdin();
    let tty = if stdin.is_terminal() {
        None
    } else {
        Some(
            std::fs::File::options()
                .read(true)
                .write(true)
                .open("/dev/tty")?,
        )
    };
    let fd = tty
        .as_ref()
        .map_or_else(|| stdin.as_fd(), |tty| tty.as_fd());
    let mut mode = tcgetattr(fd)?;
    // Crossterm retries reads while parsing incomplete UTF-8 and escape sequences.
    // Poll controls waiting; an empty follow-up read must return immediately.
    mode.special_codes[SpecialCodeIndex::VMIN] = 0;
    mode.special_codes[SpecialCodeIndex::VTIME] = 0;
    Ok(tcsetattr(fd, OptionalActions::Now, &mode)?)
}

#[cfg(not(unix))]
pub(crate) fn configure_read() -> io::Result<()> {
    Ok(())
}
