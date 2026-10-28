use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const GETTEXT_PACKAGE: &str = "bigrecorder";

pub fn init() {
    apply_language_environment_fallback();
    let _ = gettextrs::setlocale(gettextrs::LocaleCategory::LcAll, "");
    apply_language_fallback();
    let _ = gettextrs::bindtextdomain(GETTEXT_PACKAGE, locale_dir());
    let _ = gettextrs::bind_textdomain_codeset(GETTEXT_PACKAGE, "UTF-8");
    let _ = gettextrs::textdomain(GETTEXT_PACKAGE);
}

pub fn gettext(message: &str) -> String {
    gettextrs::dgettext(GETTEXT_PACKAGE, message)
}

pub fn ngettext(singular: &str, plural: &str, n: u32) -> String {
    gettextrs::dngettext(GETTEXT_PACKAGE, singular, plural, n)
}

/// Replaces each placeholder key in a single pass, so substituted values that
/// happen to contain another key (e.g. a file named "{count}") stay intact.
pub fn format_message(message: &str, values: &[(&str, &str)]) -> String {
    let mut output = String::with_capacity(message.len());
    let mut rest = message;
    'scan: while let Some(character) = rest.chars().next() {
        for (key, value) in values {
            if !key.is_empty() && rest.starts_with(key) {
                output.push_str(value);
                rest = &rest[key.len()..];
                continue 'scan;
            }
        }
        output.push(character);
        rest = &rest[character.len_utf8()..];
    }
    output
}

fn locale_dir() -> PathBuf {
    let local_dir = PathBuf::from("locale");
    if locale_dir_has_package(&local_dir) {
        return local_dir;
    }

    if let Ok(executable) = env::current_exe()
        && let Some(parent) = executable.parent()
    {
        for directory in parent.ancestors() {
            let candidate = directory.join("locale");
            if locale_dir_has_package(&candidate) {
                return candidate;
            }

            let installed_candidate = directory.join("share/locale");
            if locale_dir_has_package(&installed_candidate) {
                return installed_candidate;
            }
        }
    }

    PathBuf::from("/usr/share/locale")
}

fn locale_dir_has_package(directory: &Path) -> bool {
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };

    entries.filter_map(Result::ok).any(|entry| {
        entry
            .path()
            .join("LC_MESSAGES")
            .join(format!("{GETTEXT_PACKAGE}.mo"))
            .is_file()
    })
}

fn apply_language_fallback() {
    if !locale_environment_is_c() {
        return;
    }

    for locale in language_locale_candidates() {
        if gettextrs::setlocale(gettextrs::LocaleCategory::LcMessages, locale.as_str()).is_some() {
            return;
        }

        if !locale.contains('.') {
            let utf8_locale = format!("{locale}.UTF-8");
            if gettextrs::setlocale(gettextrs::LocaleCategory::LcMessages, utf8_locale.as_str())
                .is_some()
            {
                return;
            }
        }
    }
}

fn apply_language_environment_fallback() {
    if !locale_environment_is_c() {
        normalize_language_environment();
        return;
    }

    let Some(locale) = fallback_locale() else {
        normalize_language_environment();
        return;
    };

    let language = language_tag(&locale);
    let mut language_chain = vec![language.clone()];
    if let Some(base_language) = language.split('_').next()
        && base_language != language
    {
        language_chain.push(base_language.to_string());
    }

    // SAFETY: i18n::init() runs at process startup before GTK/GStreamer are
    // initialized, so no other application threads are reading the environment.
    unsafe {
        env::set_var("LANG", &locale);
        env::set_var("LC_MESSAGES", &locale);
        env::set_var("LANGUAGE", language_chain.join(":"));

        if env::var("LC_ALL").is_ok_and(|value| is_c_locale(&value)) {
            env::remove_var("LC_ALL");
        }
    }
}

fn normalize_language_environment() {
    let languages = language_locale_candidates()
        .into_iter()
        .map(|locale| language_tag(&locale))
        .filter(|language| !language.is_empty())
        .collect::<Vec<_>>();

    if languages.is_empty() {
        return;
    }

    // SAFETY: i18n::init() runs at process startup before GTK/GStreamer are
    // initialized, so no other application threads are reading the environment.
    unsafe {
        env::set_var("LANGUAGE", languages.join(":"));
    }
}

fn locale_environment_is_c() -> bool {
    let locale = env::var("LC_ALL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            env::var("LC_MESSAGES")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            env::var("LANG")
                .ok()
                .filter(|value| !value.trim().is_empty())
        });

    locale.is_some_and(|value| is_c_locale(&value))
}

fn is_c_locale(value: &str) -> bool {
    let locale = value.trim().to_ascii_lowercase();
    locale == "c" || locale == "posix" || locale.starts_with("c.")
}

fn language_locale_candidates() -> Vec<String> {
    env::var("LANGUAGE")
        .ok()
        .into_iter()
        .flat_map(|value| {
            value
                .split(':')
                .map(str::trim)
                .filter(|locale| !locale.is_empty() && !is_c_locale(locale))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn fallback_locale() -> Option<String> {
    language_locale_candidates()
        .into_iter()
        .find_map(|locale| installed_locale(&locale))
        .or_else(system_locale)
        .filter(|locale| !is_c_locale(locale))
}

fn installed_locale(locale: &str) -> Option<String> {
    let candidates = if locale.contains('.') {
        vec![locale.to_string()]
    } else {
        vec![locale.to_string(), format!("{locale}.UTF-8")]
    };

    candidates.into_iter().find(|candidate| {
        gettextrs::setlocale(gettextrs::LocaleCategory::LcMessages, candidate.as_str()).is_some()
    })
}

fn system_locale() -> Option<String> {
    fs::read_to_string("/etc/locale.conf")
        .ok()
        .and_then(|content| {
            content.lines().find_map(|line| {
                let line = line.trim();
                let locale = line.strip_prefix("LANG=")?.trim_matches('"');
                (!locale.is_empty()).then(|| locale.to_string())
            })
        })
}

fn language_tag(locale: &str) -> String {
    locale
        .split('.')
        .next()
        .unwrap_or(locale)
        .split('@')
        .next()
        .unwrap_or(locale)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn format_message_substitutes_in_a_single_pass() {
        assert_eq!(
            format_message(
                "{recording} has {count} takes",
                &[("{recording}", "{count}"), ("{count}", "3")]
            ),
            "{count} has 3 takes"
        );
        assert_eq!(format_message("ação {n}", &[("{n}", "ç")]), "ação ç");
    }

    #[test]
    fn gettext_uses_package_domain_after_default_domain_changes() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let env_snapshot = EnvSnapshot::capture(&["LANG", "LANGUAGE", "LC_ALL", "LC_MESSAGES"]);
        let previous_domain = gettextrs::getters::current_textdomain().ok();

        set_test_env("LANG", "pt_BR.UTF-8");
        set_test_env("LANGUAGE", "pt_BR.UTF-8");
        set_test_env("LC_ALL", "C.UTF-8");
        remove_test_env("LC_MESSAGES");

        init();

        if gettextrs::setlocale(gettextrs::LocaleCategory::LcMessages, "pt_BR.UTF-8").is_some() {
            let _ = gettextrs::textdomain("messages");
            assert_eq!(gettext("Record"), "Gravar");
        }

        if let Some(domain) = previous_domain {
            let _ = gettextrs::textdomain(domain);
        }
        env_snapshot.restore();
    }

    #[test]
    fn locale_dir_ignores_unrelated_locale_directory() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let original_dir = env::current_dir().expect("read current dir");
        let test_dir = env::temp_dir().join(format!("bigrecorder-i18n-{}", std::process::id()));
        let unrelated_locale_dir = test_dir.join("locale");

        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&unrelated_locale_dir).expect("create test locale dir");
        env::set_current_dir(&test_dir).expect("enter test dir");

        let detected_dir = locale_dir();

        env::set_current_dir(original_dir).expect("restore current dir");
        let _ = fs::remove_dir_all(&test_dir);

        assert_ne!(detected_dir, PathBuf::from("locale"));
    }

    struct EnvSnapshot {
        values: Vec<(&'static str, Option<String>)>,
    }

    impl EnvSnapshot {
        fn capture(keys: &[&'static str]) -> Self {
            let values = keys.iter().map(|key| (*key, env::var(key).ok())).collect();
            Self { values }
        }

        fn restore(self) {
            for (key, value) in self.values {
                match value {
                    Some(value) => set_test_env(key, &value),
                    None => remove_test_env(key),
                }
            }
        }
    }

    fn set_test_env(key: &str, value: &str) {
        // SAFETY: Tests hold ENV_LOCK while mutating process environment.
        unsafe {
            env::set_var(key, value);
        }
    }

    fn remove_test_env(key: &str) {
        // SAFETY: Tests hold ENV_LOCK while mutating process environment.
        unsafe {
            env::remove_var(key);
        }
    }
}
