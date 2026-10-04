use gettextrs::gettext;

pub fn tr(text: &str) -> String {
    gettext(text)
}

/// Translates and fills `{name}` placeholders.
pub fn trf(text: &str, args: &[(&str, &str)]) -> String {
    let mut result = gettext(text);
    for (name, value) in args {
        result = result.replace(&format!("{{{name}}}"), value);
    }
    result
}

/// Like `trf`, choosing the singular or plural form by the number in the `n` argument.
pub fn trn(singular: &str, plural: &str, args: &[(&str, &str)]) -> String {
    let n = args.iter().find(|(name, _)| *name == "n").and_then(|(_, v)| v.parse::<u32>().ok()).unwrap_or(2);
    let mut result = gettextrs::ngettext(singular, plural, n);
    for (name, value) in args {
        result = result.replace(&format!("{{{name}}}"), value);
    }
    result
}
