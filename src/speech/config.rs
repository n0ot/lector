/// A speech server Lector can start without involving a shell.
///
/// `program` is passed directly to [`std::process::Command::new`] and every
/// entry in `args` becomes exactly one argument. In particular, whitespace,
/// quotes, and shell metacharacters are not parsed or expanded by Lector.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum SpeechServerSpec {
    #[default]
    Native,
    Process {
        program: String,
        args: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::SpeechServerSpec;

    #[test]
    fn native_is_the_default() {
        assert_eq!(SpeechServerSpec::default(), SpeechServerSpec::Native);
    }
}
