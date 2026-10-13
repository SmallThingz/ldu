use crate::{backend::Backend, theme::ThemeColor};
use std::{
    ffi::OsString,
    fs::OpenOptions,
    io::{self, Read},
    path::{Path, PathBuf},
};

pub const MAX_DEBOUNCE_MS: u64 = 60_000;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub color: ThemeColor,
    pub mouse: bool,
    pub mouse_debounce_ms: u64,
    pub mouse_debounce_delay_ms: u64,
    pub mouse_debounce_warning: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            color: ThemeColor::default(),
            mouse: true,
            mouse_debounce_ms: 250,
            mouse_debounce_delay_ms: 1_250,
            mouse_debounce_warning: true,
        }
    }
}

#[derive(Default)]
pub struct Config {
    pub ui: Settings,
    pub threads: Option<usize>,
    pub backend: Option<Backend>,
    pub buffer_size: Option<usize>,
    pub batch_size: Option<usize>,
    pub apparent_size: Option<bool>,
}

fn config_path(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    xdg.map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(".config"))
        })
        .map(|path| path.join("ldu/config"))
}

pub fn load() -> io::Result<Config> {
    config_path(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
    .map_or_else(|| Ok(Config::default()), |path| load_path(&path))
}

fn load_path(path: &Path) -> io::Result<Config> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Inspect the opened file without hanging when the config is a FIFO.
        options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(file_error(path, error)),
    };
    if !file
        .metadata()
        .map_err(|error| file_error(path, error))?
        .is_file()
    {
        return Err(invalid(path, 0, "configuration must be a regular file"));
    }
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|error| file_error(path, error))?;
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err(invalid(path, 0, "configuration exceeds 64 KiB"));
    }
    parse(&text, path)
}

fn file_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

fn invalid(path: &Path, line: usize, message: impl std::fmt::Display) -> io::Error {
    let location = if line == 0 {
        path.display().to_string()
    } else {
        format!("{}:{line}", path.display())
    };
    io::Error::new(io::ErrorKind::InvalidData, format!("{location}: {message}"))
}

fn parse(text: &str, path: &Path) -> io::Result<Config> {
    let mut config = Config::default();
    let mut seen = 0u16;
    for (offset, raw) in text.lines().enumerate() {
        let line = offset + 1;
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let (key, value) = raw
            .split_once('=')
            .ok_or_else(|| invalid(path, line, "expected key = value"))?;
        let key = key.trim();
        let value = value.trim();
        let value = if value.starts_with(['\'', '"']) {
            let quote = value.as_bytes()[0];
            if value.len() < 2 || value.as_bytes().last() != Some(&quote) {
                return Err(invalid(path, line, "unterminated quoted value"));
            }
            &value[1..value.len() - 1]
        } else {
            value
        };
        let error = |message: &str| invalid(path, line, format!("{key}: {message}"));
        let boolean = || match value {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(error("expected true or false")),
        };
        let number = |min: u64, max: u64| {
            value
                .parse::<u64>()
                .ok()
                .filter(|value| (min..=max).contains(value))
                .ok_or_else(|| error(&format!("expected an integer from {min} to {max}")))
        };
        let bit = match key {
            "color" => {
                config.ui.color = value
                    .parse()
                    .map_err(|err| invalid(path, line, format!("color: {err}")))?;
                1
            }
            "mouse" => {
                config.ui.mouse = boolean()?;
                1 << 1
            }
            "mouse_debounce_ms" | "mouse-debounce-ms" => {
                config.ui.mouse_debounce_ms = number(0, MAX_DEBOUNCE_MS)?;
                1 << 2
            }
            "mouse_debounce_delay_ms" | "mouse-debounce-delay-ms" => {
                config.ui.mouse_debounce_delay_ms = number(0, MAX_DEBOUNCE_MS)?;
                1 << 3
            }
            "mouse_debounce_warning" | "mouse-debounce-warning" => {
                config.ui.mouse_debounce_warning = boolean()?;
                1 << 4
            }
            "threads" => {
                config.threads = Some(number(1, 128)? as usize);
                1 << 5
            }
            "backend" => {
                config.backend = Some(match value {
                    "auto" => Backend::Auto,
                    "native" => Backend::Native,
                    "std" => Backend::Std,
                    _ => return Err(error("expected auto, native, or std")),
                });
                1 << 6
            }
            "buffer_size" | "buffer-size" => {
                config.buffer_size = Some(number(4096, 4_194_304)? as usize);
                1 << 7
            }
            "batch_size" | "batch-size" => {
                config.batch_size = Some(number(1, 4096)? as usize);
                1 << 8
            }
            "apparent_size" | "apparent-size" => {
                config.apparent_size = Some(boolean()?);
                1 << 9
            }
            _ => return Err(invalid(path, line, format!("unknown setting {key:?}"))),
        };
        if seen & bit != 0 {
            return Err(invalid(path, line, format!("duplicate setting {key:?}")));
        }
        seen |= bit;
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Config {
        parse(text, Path::new("/fixture/ldu/config")).unwrap()
    }

    #[test]
    fn xdg_paths_require_absolute_directories_and_fall_back_to_home() {
        let path = |xdg: Option<&str>, home: Option<&str>| {
            config_path(xdg.map(Into::into), home.map(Into::into))
        };
        assert_eq!(
            path(Some("/xdg"), Some("/home/me")),
            Some("/xdg/ldu/config".into())
        );
        for xdg in [None, Some(""), Some("relative/config")] {
            assert_eq!(
                path(xdg, Some("/home/me")),
                Some("/home/me/.config/ldu/config".into())
            );
        }
        assert_eq!(path(None, None), None);
        assert_eq!(path(Some("relative"), Some("relative")), None);
        assert_eq!(path(Some("/xdg"), None), Some("/xdg/ldu/config".into()));
    }

    #[test]
    fn config_parses_all_settings_without_new_serialization_dependencies() {
        let config = parsed(
            "# example\n\ncolor = '#1256ab'\nmouse = false\nmouse_debounce_ms=0\nmouse_debounce_delay_ms=60000\nmouse_debounce_warning=false\nthreads=7\nbackend=native\nbuffer_size=8192\nbatch_size=64\napparent_size=true\n",
        );
        assert_eq!(config.ui.color, "#1256ab".parse().unwrap());
        assert!(!config.ui.mouse);
        assert_eq!(config.ui.mouse_debounce_ms, 0);
        assert_eq!(config.ui.mouse_debounce_delay_ms, MAX_DEBOUNCE_MS);
        assert!(!config.ui.mouse_debounce_warning);
        assert_eq!(config.threads, Some(7));
        assert!(matches!(config.backend, Some(Backend::Native)));
        assert_eq!(config.buffer_size, Some(8192));
        assert_eq!(config.batch_size, Some(64));
        assert_eq!(config.apparent_size, Some(true));
        assert_eq!(
            parsed("color = \"red\"\r\nmouse-debounce-ms = 19")
                .ui
                .mouse_debounce_ms,
            19
        );
        assert_eq!(parsed("").ui, Settings::default());
    }

    #[test]
    fn malformed_config_identifies_file_and_line() {
        for bad in [
            "color = #12345",
            "color = 'red",
            "mouse = yes",
            "mouse_debounce_ms = -1",
            "mouse_debounce_delay_ms = 60001",
            "mouse_debounce_ms = 18446744073709551615",
            "mouse_debounce_warning = 0",
            "threads = 0",
            "threads = 129",
            "backend = fast",
            "buffer_size = 4095",
            "batch_size = 4097",
            "apparent_size = nope",
            "unknown = true",
            "mouse false",
            "color = 256",
        ] {
            let error = parse(
                &format!("# example\n{bad}"),
                Path::new("/fixture/ldu/config"),
            )
            .err()
            .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(
                error.to_string().starts_with("/fixture/ldu/config:2:"),
                "{error}"
            );
        }
        let error = parse(
            "mouse_debounce_ms=1\nmouse-debounce-ms=2",
            Path::new("config"),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("config:2: duplicate setting"));
    }

    #[test]
    fn missing_config_is_optional_but_invalid_or_oversized_files_fail() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("config");
        assert_eq!(load_path(&path).unwrap().ui, Settings::default());
        std::fs::write(&path, b"mouse=false\n").unwrap();
        assert!(!load_path(&path).unwrap().ui.mouse);
        std::fs::write(&path, [0xff]).unwrap();
        let error = load_path(&path).err().unwrap();
        assert!(error.to_string().contains(path.to_str().unwrap()));
        std::fs::write(&path, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
        assert!(
            load_path(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("exceeds 64 KiB")
        );
        assert!(
            load_path(fixture.path())
                .err()
                .unwrap()
                .to_string()
                .contains("regular file")
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_fifo_is_rejected_without_waiting_for_a_writer() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("config");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        assert!(
            load_path(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("regular file")
        );
    }
}
