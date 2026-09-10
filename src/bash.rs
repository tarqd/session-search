//! Structured view of a Bash `command` string, for the `bash_cmd` index field.
//!
//! [`extract`] parses the script with `brush-parser` and flattens it into two lists:
//!
//! * `program` — `argv[0]` of *every* simple command in the script, in source order:
//!   inside pipelines, `&&`/`||` chains, subshells, brace groups, loop and conditional
//!   bodies, function bodies, coprocesses and process substitutions.
//! * `args` — every suffix word of every simple command (flags included), flattened
//!   across all commands in the same order.
//!
//! Nothing is expanded. Words are the raw source text with one layer of matching outer
//! quotes removed, so `$(uname -a)` and `$HOME` stay opaque. Assignment prefixes
//! (`FOO=bar cmd`), redirection operators and their targets, here-document bodies and
//! here-strings are not args — but a process substitution used as a redirect target is
//! still descended into, because it contains real commands.
//!
//! This is deliberately lossy: it exists so `bash_cmd.program:cargo` can find every
//! transcript where `cargo` ran anywhere in a shell one-liner, which searching the raw
//! command text cannot do without also matching `--cargo-flag` or a path segment.

use brush_parser::ast;

/// Bound on the AST walk. This is a backstop, not the real depth defence: [`too_deep`]
/// rejects deeply nested *source* before the parser ever sees it, because the parser
/// recurses (and dies) long before a walk of what it returns would.
const MAX_DEPTH: usize = 128;

/// Nesting depth of `(` / `{` in the raw source above which we do not even try to parse.
///
/// Two separate parser failure modes live above this line, both reachable from one weird
/// transcript and both fatal to a whole indexing run:
///
/// * brush's PEG backtracks exponentially over a run of unmatched `(` — 26 bytes of `(`
///   takes ~15s, 30 bytes takes minutes, and the answer is `None` either way.
/// * deeply *balanced* nesting recurses until the thread's stack is gone, which aborts the
///   process: a stack overflow is not an unwinding panic, so [`parse`]'s `catch_unwind`
///   cannot catch it.
///
/// Real commands nest a handful of levels, so a small limit costs nothing: rejecting such a
/// script with `None` is the same answer the parser would have given, minus the cost.
const MAX_SOURCE_DEPTH: i32 = 16;

/// A parsed Bash command line: the programs it runs and the arguments they get.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BashCmd {
    /// `argv[0]` of every simple command in the script, in source order.
    pub program: Vec<String>,
    /// Every suffix word of every simple command, in source order.
    pub args: Vec<String>,
}

impl BashCmd {
    /// The JSON stored in (and indexed by) the `bash_cmd` field.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "program": self.program, "args": self.args })
    }
}

/// Parse `command` as a Bash script and summarise it.
///
/// Returns `None` when the script does not parse, when the parser panics, when its source
/// nests deeper than [`MAX_SOURCE_DEPTH`] (see [`too_deep`]), or when it parses but contains
/// no simple command with a program word (an empty script, a lone comment, `((1+2))`).
/// There is no heuristic fallback: a `None` here means the document simply carries no
/// `bash_cmd`, which is better than a guess that would make `--program` lie.
pub fn extract(command: &str) -> Option<BashCmd> {
    if command.trim().is_empty() {
        return None;
    }

    if too_deep(command) {
        return None;
    }

    let program = parse(command)?;

    let mut c = Collector::default();
    for complete_command in &program.complete_commands {
        c.list(complete_command);
    }

    if c.cmd.program.is_empty() {
        return None;
    }
    Some(c.cmd)
}

/// True when `command` nests `(` or `{` deeper than [`MAX_SOURCE_DEPTH`] outside quotes.
///
/// A byte scan, deliberately crude — it costs nanoseconds and only has to be right about
/// *shell-significant* delimiters. Quoted text is skipped so an `awk '{ if ((a)) ... }'`
/// program or a `python -c "print(f(g(x)))"` one-liner keeps its `bash_cmd`, and backslash
/// escapes are honoured so `echo it\'s fine` does not desync the quote state. A here-document
/// body is scanned too, since it is raw text to this pass — harmless, because source code
/// brackets balance, so a `cat > f <<EOF` of a real file tracks its own indentation depth.
fn too_deep(command: &str) -> bool {
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    for b in command.bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, b) {
            // A single-quoted string ends only at the next `'`; nothing escapes inside it.
            (Some(b'\''), b'\'') => quote = None,
            (Some(b'\''), _) => {}
            (Some(b'"'), b'\\') => escaped = true,
            (Some(b'"'), b'"') => quote = None,
            (Some(_), _) => {}
            (None, b'\\') => escaped = true,
            (None, b'\'' | b'"') => quote = Some(b),
            (None, b'(' | b'{') => {
                depth += 1;
                if depth > MAX_SOURCE_DEPTH {
                    return true;
                }
            }
            (None, b')' | b'}') => depth = (depth - 1).max(0),
            _ => {}
        }
    }
    false
}

/// Run `brush-parser` over `command`, swallowing both parse errors and parser panics.
///
/// The panic guard is not paranoia about our own code: the parser is a PEG over arbitrary
/// transcript text, and a panic on some input would otherwise take down a whole indexing
/// run over one weird command. It does *not* cover a stack overflow, which aborts the
/// process rather than unwinding — that is [`too_deep`]'s job, upstream of here.
fn parse(command: &str) -> Option<ast::Program> {
    let options = brush_parser::ParserOptions::default();
    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut parser =
            brush_parser::Parser::new(std::io::Cursor::new(command.as_bytes()), &options);
        parser.parse_program()
    }));
    match parsed {
        Ok(Ok(program)) => Some(program),
        // Parse error, or a panic inside the parser.
        _ => None,
    }
}

/// Strip one layer of matching outer quotes. No expansion, no escape processing.
///
/// A trailing `\r` goes first: on a CRLF transcript the parser treats the CR as an ordinary
/// word character, and since `bash_cmd` is tokenized `raw` a `"cargo\r"` term is invisible to
/// `--program cargo` and shows up as a duplicate facet bucket next to the plain `cargo` one.
fn unquote(raw: &str) -> &str {
    let raw = raw.strip_suffix('\r').unwrap_or(raw);
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'\'' || first == b'"') && first == last {
            return &raw[1..raw.len() - 1];
        }
    }
    raw
}

#[derive(Default)]
struct Collector {
    cmd: BashCmd,
    depth: usize,
}

impl Collector {
    fn push_program(&mut self, word: &ast::Word) {
        let value = unquote(&word.value);
        if !value.is_empty() {
            self.cmd.program.push(value.to_owned());
        }
    }

    fn push_arg(&mut self, word: &ast::Word) {
        let value = unquote(&word.value);
        if !value.is_empty() {
            self.cmd.args.push(value.to_owned());
        }
    }

    /// Recurse one level deeper, unless we are already too deep.
    fn nested(&mut self, f: impl FnOnce(&mut Self)) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.depth += 1;
        f(self);
        self.depth -= 1;
    }

    fn list(&mut self, list: &ast::CompoundList) {
        for item in &list.0 {
            self.and_or_list(&item.0);
        }
    }

    fn and_or_list(&mut self, list: &ast::AndOrList) {
        self.pipeline(&list.first);
        for item in &list.additional {
            match item {
                ast::AndOr::And(pipeline) | ast::AndOr::Or(pipeline) => self.pipeline(pipeline),
            }
        }
    }

    fn pipeline(&mut self, pipeline: &ast::Pipeline) {
        for command in &pipeline.seq {
            self.command(command);
        }
    }

    fn command(&mut self, command: &ast::Command) {
        match command {
            ast::Command::Simple(simple) => self.simple(simple),
            ast::Command::Compound(compound, redirects) => {
                self.compound(compound);
                if let Some(ast::RedirectList(redirects)) = redirects {
                    for redirect in redirects {
                        self.redirect(redirect);
                    }
                }
            }
            // The function *name* is not a program being run; its body's commands are.
            ast::Command::Function(function) => {
                self.compound(&function.body.0);
                if let Some(ast::RedirectList(redirects)) = &function.body.1 {
                    for redirect in redirects {
                        self.redirect(redirect);
                    }
                }
            }
            // `[[ ... ]]` runs no program and its words are test operands, not args — but
            // its redirect list can still be `> >(tee log)`, which does run one.
            ast::Command::ExtendedTest(_, redirects) => {
                if let Some(ast::RedirectList(redirects)) = redirects {
                    for redirect in redirects {
                        self.redirect(redirect);
                    }
                }
            }
        }
    }

    fn compound(&mut self, compound: &ast::CompoundCommand) {
        self.nested(|c| match compound {
            ast::CompoundCommand::BraceGroup(group) => c.list(&group.list),
            ast::CompoundCommand::Subshell(subshell) => c.list(&subshell.list),
            // `for f in a b` — the values are loop data, not arguments to a command.
            ast::CompoundCommand::ForClause(clause) => c.list(&clause.body.list),
            ast::CompoundCommand::ArithmeticForClause(clause) => c.list(&clause.body.list),
            // Likewise the case subject and its patterns.
            ast::CompoundCommand::CaseClause(clause) => {
                for case in &clause.cases {
                    if let Some(body) = &case.cmd {
                        c.list(body);
                    }
                }
            }
            ast::CompoundCommand::IfClause(clause) => {
                c.list(&clause.condition);
                c.list(&clause.then);
                for else_clause in clause.elses.iter().flatten() {
                    if let Some(condition) = &else_clause.condition {
                        c.list(condition);
                    }
                    c.list(&else_clause.body);
                }
            }
            ast::CompoundCommand::WhileClause(clause)
            | ast::CompoundCommand::UntilClause(clause) => {
                c.list(&clause.0);
                c.list(&clause.1.list);
            }
            // The coprocess name is a shell-level label, not a program.
            ast::CompoundCommand::Coprocess(coproc) => c.command(&coproc.body),
            // Arithmetic commands run nothing.
            ast::CompoundCommand::Arithmetic(_) => {}
        });
    }

    fn simple(&mut self, simple: &ast::SimpleCommand) {
        if let Some(word) = &simple.word_or_name {
            self.push_program(word);
        }
        if let Some(ast::CommandPrefix(items)) = &simple.prefix {
            self.items(items, false);
        }
        if let Some(ast::CommandSuffix(items)) = &simple.suffix {
            self.items(items, true);
        }
    }

    /// Walk prefix or suffix items. Only suffix words are arguments; a prefix holds
    /// assignments and redirections, whose words are neither program nor argument.
    fn items(&mut self, items: &[ast::CommandPrefixOrSuffixItem], words_are_args: bool) {
        for item in items {
            match item {
                ast::CommandPrefixOrSuffixItem::Word(word) => {
                    if words_are_args {
                        self.push_arg(word);
                    }
                }
                // The grammar reports any `NAME=value`-shaped word as an assignment, wherever
                // it sits, so position decides what it means: in the prefix it is `FOO=bar
                // cmd` environment and not an argument, but in the suffix (`docker run -e
                // FOO=bar img`, `make CFLAGS=-O2 all`, `echo a=b`) it is a plain argument. The
                // `Word` here carries the whole raw `NAME=value` text.
                ast::CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
                    if words_are_args {
                        self.push_arg(word);
                    }
                }
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, subshell) => {
                    self.nested(|c| c.list(&subshell.list));
                }
                ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => self.redirect(redirect),
            }
        }
    }

    /// A redirection contributes no words — but `> >(tee log)` contains a command.
    fn redirect(&mut self, redirect: &ast::IoRedirect) {
        if let ast::IoRedirect::File(
            _,
            _,
            ast::IoFileRedirectTarget::ProcessSubstitution(_, subshell),
        ) = redirect
        {
            self.nested(|c| c.list(&subshell.list));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(program, args)` as string slices, for compact assertions.
    fn parts(command: &str) -> (Vec<String>, Vec<String>) {
        let cmd = extract(command).unwrap_or_else(|| panic!("no bash_cmd for {command:?}"));
        (cmd.program, cmd.args)
    }

    fn program(command: &str) -> Vec<String> {
        parts(command).0
    }

    fn args(command: &str) -> Vec<String> {
        parts(command).1
    }

    #[test]
    fn simple_command() {
        assert_eq!(
            parts("ls -la /tmp"),
            (vec!["ls".into()], vec!["-la".into(), "/tmp".into()])
        );
    }

    #[test]
    fn bare_command_has_no_args() {
        assert_eq!(parts("pwd"), (vec!["pwd".into()], vec![]));
    }

    #[test]
    fn pipeline() {
        assert_eq!(
            parts("grep -r foo . | sort | wc -l"),
            (
                vec!["grep".into(), "sort".into(), "wc".into()],
                vec!["-r".into(), "foo".into(), ".".into(), "-l".into()]
            )
        );
    }

    #[test]
    fn and_or_and_sequence_chains() {
        assert_eq!(
            program("a 1 && b 2 || c 3 ; d 4"),
            vec!["a".to_string(), "b".into(), "c".into(), "d".into()]
        );
        assert_eq!(
            args("a 1 && b 2 || c 3 ; d 4"),
            vec!["1".to_string(), "2".into(), "3".into(), "4".into()]
        );
    }

    #[test]
    fn background_separator() {
        assert_eq!(
            program("sleep 1 & wait"),
            vec!["sleep".to_string(), "wait".into()]
        );
    }

    /// The worked example from the design: redirections drop out, everything else keeps
    /// its source order across the `&&` and the pipe.
    #[test]
    fn pipeline_with_redirect_is_the_pinned_example() {
        let (program, args) = parts("cd X && cargo test 2>&1 | tail -5");
        assert_eq!(
            program,
            vec!["cd".to_string(), "cargo".into(), "tail".into()]
        );
        assert_eq!(args, vec!["X".to_string(), "test".into(), "-5".into()]);
        assert!(!args.iter().any(|a| a.contains("2>&1") || a == "1"));
    }

    #[test]
    fn redirect_targets_are_not_args() {
        assert_eq!(
            parts("echo hi > out.txt"),
            (vec!["echo".into()], vec!["hi".into()])
        );
        assert_eq!(
            parts("sort < in.txt > out.txt"),
            (vec!["sort".into()], vec![])
        );
        assert_eq!(parts("cmd 2>>log"), (vec!["cmd".into()], vec![]));
        assert_eq!(parts("cmd &> both.txt"), (vec!["cmd".into()], vec![]));
        assert_eq!(parts("cmd >&2"), (vec!["cmd".into()], vec![]));
        assert_eq!(
            parts("cmd <<< \"here string\""),
            (vec!["cmd".into()], vec![])
        );
    }

    #[test]
    fn redirect_before_the_command_name() {
        assert_eq!(
            parts("> out.txt echo hi"),
            (vec!["echo".into()], vec!["hi".into()])
        );
    }

    #[test]
    fn heredoc_body_is_not_args() {
        let (program, args) = parts("cat > f <<'EOF'\nbody words\nEOF\n");
        assert_eq!(program, vec!["cat".to_string()]);
        assert_eq!(args, Vec::<String>::new());
        assert!(
            !program
                .iter()
                .chain(args.iter())
                .any(|w| w.contains("body") || w.contains("words"))
        );
    }

    #[test]
    fn heredoc_with_a_real_command_after_it() {
        let (program, args) = parts("cat <<EOF\nrm -rf /\nEOF\necho done");
        assert_eq!(program, vec!["cat".to_string(), "echo".into()]);
        assert_eq!(args, vec!["done".to_string()]);
    }

    #[test]
    fn assignment_prefix_is_not_a_program_or_arg() {
        assert_eq!(parts("FOO=bar cmd"), (vec!["cmd".into()], vec![]));
        assert_eq!(
            parts("a=1 b=2 env -i"),
            (vec!["env".into()], vec!["-i".into()])
        );
    }

    #[test]
    fn bare_assignment_has_no_program() {
        assert_eq!(extract("FOO=bar"), None);
    }

    /// Position, not shape, decides: the grammar calls every `NAME=value` word an
    /// assignment, but one *after* the command name is an ordinary argument.
    #[test]
    fn assignment_shaped_words_after_the_command_name_are_args() {
        assert_eq!(parts("echo a=b"), (vec!["echo".into()], vec!["a=b".into()]));
        assert_eq!(
            args("docker run -e FOO=bar image"),
            vec![
                "run".to_string(),
                "-e".into(),
                "FOO=bar".into(),
                "image".into()
            ]
        );
        assert_eq!(
            args("make CFLAGS=-O2 all"),
            vec!["CFLAGS=-O2".to_string(), "all".into()]
        );
        assert_eq!(
            args("env FOO=bar cargo build"),
            vec!["FOO=bar".to_string(), "cargo".into(), "build".into()]
        );
        assert_eq!(
            parts("export PATH=$PATH:/x && which foo"),
            (
                vec!["export".into(), "which".into()],
                vec!["PATH=$PATH:/x".into(), "foo".into()]
            )
        );
    }

    /// A CRLF transcript must not produce `"cargo\r"`, which the `raw` tokenizer indexes as
    /// a term no `--program cargo` can ever match.
    #[test]
    fn carriage_returns_are_trimmed_from_words() {
        assert_eq!(
            program("cargo\r\nls\r\n"),
            vec!["cargo".to_string(), "ls".into()]
        );
        assert_eq!(
            parts("cd /tmp\r\ncargo test\r\n"),
            (
                vec!["cd".into(), "cargo".into()],
                vec!["/tmp".into(), "test".into()]
            )
        );
        // Quotes still come off after the CR does.
        assert_eq!(args("echo \"hi there\"\r\n"), vec!["hi there".to_string()]);
    }

    #[test]
    fn quotes_are_stripped_one_layer() {
        assert_eq!(
            args("echo \"hello world\""),
            vec!["hello world".to_string()]
        );
        assert_eq!(args("echo 'a b'"), vec!["a b".to_string()]);
        assert_eq!(args("echo \"it's\""), vec!["it's".to_string()]);
        // One layer only: the inner quotes survive.
        assert_eq!(args("echo \"'nested'\""), vec!["'nested'".to_string()]);
        // A quoted program word is stripped the same way.
        assert_eq!(program("'my prog' x"), vec!["my prog".to_string()]);
    }

    #[test]
    fn empty_words_are_dropped() {
        assert_eq!(parts("ls '' \"\" x"), (vec!["ls".into()], vec!["x".into()]));
    }

    #[test]
    fn subshell() {
        assert_eq!(
            parts("( cd /tmp && ls -l )"),
            (
                vec!["cd".into(), "ls".into()],
                vec!["/tmp".into(), "-l".into()]
            )
        );
    }

    #[test]
    fn brace_group() {
        assert_eq!(
            parts("{ echo a; echo b; }"),
            (
                vec!["echo".into(), "echo".into()],
                vec!["a".into(), "b".into()]
            )
        );
    }

    #[test]
    fn brace_group_with_a_redirect_list() {
        assert_eq!(
            parts("{ echo a; } > log"),
            (vec!["echo".into()], vec!["a".into()])
        );
    }

    #[test]
    fn if_then_else() {
        assert_eq!(
            parts("if test -f x; then echo yes; elif grep q f; then echo maybe; else echo no; fi"),
            (
                vec![
                    "test".into(),
                    "echo".into(),
                    "grep".into(),
                    "echo".into(),
                    "echo".into()
                ],
                vec![
                    "-f".into(),
                    "x".into(),
                    "yes".into(),
                    "q".into(),
                    "f".into(),
                    "maybe".into(),
                    "no".into()
                ]
            )
        );
    }

    #[test]
    fn for_loop_body_but_not_its_values() {
        let (program, args) = parts("for f in a.txt b.txt; do wc -l $f; done");
        assert_eq!(program, vec!["wc".to_string()]);
        assert_eq!(args, vec!["-l".to_string(), "$f".into()]);
        assert!(!args.iter().any(|a| a == "a.txt"));
    }

    #[test]
    fn arithmetic_for_loop_body() {
        assert_eq!(
            program("for ((i=0; i<3; i++)); do echo $i; done"),
            vec!["echo".to_string()]
        );
    }

    #[test]
    fn while_and_until_loops() {
        assert_eq!(
            parts("while read line; do echo $line; done"),
            (
                vec!["read".into(), "echo".into()],
                vec!["line".into(), "$line".into()]
            )
        );
        assert_eq!(
            program("until test -e f; do sleep 1; done"),
            vec!["test".to_string(), "sleep".into()]
        );
    }

    #[test]
    fn case_body_but_not_subject_or_patterns() {
        let (program, args) = parts("case $x in a) echo A;; b|c) run -v;; esac");
        assert_eq!(program, vec!["echo".to_string(), "run".into()]);
        assert_eq!(args, vec!["A".to_string(), "-v".into()]);
        assert!(!args.iter().any(|a| a == "$x" || a == "b"));
    }

    #[test]
    fn function_definition_body() {
        let (program, args) = parts("myfn() { echo inner -q; }");
        assert_eq!(program, vec!["echo".to_string()]);
        assert_eq!(args, vec!["inner".to_string(), "-q".into()]);
        // The function name is a definition, not a program that ran.
        assert!(!program.iter().any(|p| p == "myfn"));
    }

    #[test]
    fn function_definition_and_call() {
        assert_eq!(
            program("build() { cargo build; }; build"),
            vec!["cargo".to_string(), "build".into()]
        );
    }

    #[test]
    fn process_substitution() {
        let (program, args) = parts("diff <(sort a) <(sort b)");
        assert_eq!(
            program,
            vec!["diff".to_string(), "sort".into(), "sort".into()]
        );
        assert_eq!(args, vec!["a".to_string(), "b".into()]);
    }

    #[test]
    fn process_substitution_as_a_redirect_target() {
        assert_eq!(
            program("cargo test > >(tee log)"),
            vec!["cargo".to_string(), "tee".into()]
        );
    }

    /// `[[ ... ]]` runs nothing, but a process substitution in its redirect list does.
    #[test]
    fn extended_test_redirect_target_is_descended_into() {
        assert_eq!(
            parts("[[ -f x ]] > >(tee log)"),
            (vec!["tee".into()], vec!["log".into()])
        );
        assert_eq!(
            program("[[ -f x ]] > >(tee log) && echo z"),
            vec!["tee".to_string(), "echo".into()]
        );
        // A plain redirect target is still not a program, and the test operands are not args.
        assert_eq!(extract("[[ -f x ]] > out.txt"), None);
    }

    #[test]
    fn coprocess() {
        assert_eq!(
            parts("coproc mycoproc { echo hi; }"),
            (vec!["echo".into()], vec!["hi".into()])
        );
        assert_eq!(
            parts("coproc echo hi"),
            (vec!["echo".into()], vec!["hi".into()])
        );
    }

    #[test]
    fn command_substitution_stays_opaque() {
        let (progs, opaque) = parts("echo $(uname -a)");
        assert_eq!(progs, vec!["echo".to_string()]);
        assert_eq!(opaque, vec!["$(uname -a)".to_string()]);
        // Not descended into: `uname` is not a program we claim ran.
        assert!(!progs.iter().any(|p| p == "uname"));

        assert_eq!(args("echo `date +%s`"), vec!["`date +%s`".to_string()]);
        assert_eq!(
            args("cd \"$(git rev-parse --show-toplevel)\""),
            vec!["$(git rev-parse --show-toplevel)".to_string()]
        );
    }

    #[test]
    fn timed_and_negated_pipelines() {
        assert_eq!(program("time cargo build"), vec!["cargo".to_string()]);
        assert_eq!(program("! grep -q x f"), vec!["grep".to_string()]);
    }

    #[test]
    fn comments_are_stripped_by_the_parser() {
        let (program, args) = parts("# set things up\nls -l  # trailing note\n");
        assert_eq!(program, vec!["ls".to_string()]);
        assert_eq!(args, vec!["-l".to_string()]);
    }

    #[test]
    fn nested_compounds_keep_source_order() {
        assert_eq!(
            program("if true; then ( cd d && { make -j4; } ) | tee log; fi"),
            vec!["true".to_string(), "cd".into(), "make".into(), "tee".into()]
        );
    }

    #[test]
    fn empty_and_whitespace_input_is_none() {
        assert_eq!(extract(""), None);
        assert_eq!(extract("   "), None);
        assert_eq!(extract(" \n\t \n"), None);
    }

    #[test]
    fn scripts_that_run_nothing_are_none() {
        assert_eq!(extract("# only a comment"), None);
        assert_eq!(extract("((1 + 2))"), None);
        assert_eq!(extract("[[ -f x ]]"), None);
    }

    #[test]
    fn unparseable_input_is_none() {
        // Unterminated single quote: brush rejects this outright.
        assert_eq!(extract("echo 'unterminated"), None);
        // Unterminated double quote, and an unclosed compound command.
        assert_eq!(extract("echo \"unterminated"), None);
        assert_eq!(extract("if true; then"), None);
        assert_eq!(extract("for f in"), None);
        assert_eq!(extract("| pipe first"), None);
    }

    #[test]
    fn to_json_shape() {
        let cmd = extract("cd X && cargo test 2>&1 | tail -5").expect("parses");
        assert_eq!(
            cmd.to_json(),
            serde_json::json!({
                "program": ["cd", "cargo", "tail"],
                "args": ["X", "test", "-5"],
            })
        );
    }

    #[test]
    fn to_json_keeps_both_keys_when_there_are_no_args() {
        let json = extract("pwd").expect("parses").to_json();
        assert_eq!(json, serde_json::json!({ "program": ["pwd"], "args": [] }));
        assert!(json.get("args").is_some_and(serde_json::Value::is_array));
    }

    /// A long *flat* script is the realistic shape of a big Bash tool call, and it must
    /// not put 4000 frames on the stack: the list walk is iterative.
    #[test]
    fn long_flat_script_does_not_blow_the_stack() {
        let mut script = String::new();
        for i in 0..4000 {
            script.push_str(&format!("echo line{i} >> /tmp/out\n"));
        }
        let cmd = extract(&script).expect("parses");
        assert_eq!(cmd.program.len(), 4000);
        assert_eq!(cmd.args.len(), 4000);
        assert_eq!(cmd.program[0], "echo");
        assert_eq!(cmd.program[3999], "echo");
        assert_eq!(cmd.args[3999], "line3999");
    }

    /// The same size as one pipeline, which exercises the per-command path instead.
    #[test]
    fn long_pipeline_does_not_blow_the_stack() {
        let script = std::iter::repeat_n("cat", 2000)
            .collect::<Vec<_>>()
            .join(" | ");
        let cmd = extract(&script).expect("parses");
        assert_eq!(cmd.program.len(), 2000);
    }

    /// Deep nesting is bounded rather than fatal: whatever the parser accepts, we walk
    /// at most `MAX_DEPTH` levels of it and still return a usable answer.
    #[test]
    fn deep_nesting_is_truncated_not_fatal() {
        let depth = MAX_DEPTH + 20;
        let script = format!("{}echo deep{}", "( ".repeat(depth), " )".repeat(depth));
        // Either we reject it up front or we truncate; neither may panic or hang.
        if let Some(cmd) = extract(&script) {
            assert!(cmd.program.iter().all(|p| p == "echo"));
        }
    }

    /// The two inputs that kill an indexing run if they reach brush: a run of unmatched `(`
    /// (exponential PEG backtracking) and deep balanced nesting (stack overflow, which
    /// `catch_unwind` cannot catch). Both must be `None`, and fast.
    #[test]
    fn pathological_nesting_is_rejected_before_parsing() {
        // Left unguarded this takes ~15s in a debug build, and the answer is None anyway.
        assert!(too_deep(&"(".repeat(26)));
        assert_eq!(extract(&"(".repeat(26)), None);
        assert_eq!(extract(&("( ".repeat(22) + "echo")), None);
        // Balanced, but 500 levels of it overflows the parser's stack.
        let braces = "{ ".repeat(500) + "echo x;" + &" };".repeat(500);
        assert!(too_deep(&braces));
        assert_eq!(extract(&braces), None);
        let subs = format!("echo {}x{}", "$(".repeat(1000), ")".repeat(1000));
        assert_eq!(extract(&subs), None);
    }

    /// The guard scans raw bytes, so it must not count delimiters inside quotes: a real
    /// one-liner full of parens has to keep its `bash_cmd`.
    #[test]
    fn realistic_nesting_survives_the_depth_guard() {
        assert!(!too_deep(
            "python3 -c \"print(f(g(h(i(j(k(l(m(n(o(p(q(x))))))))))))))\""
        ));
        assert_eq!(
            program("python3 -c \"print(f(g(h(i(j(k(l(m(n(o(p(q(x))))))))))))))\""),
            vec!["python3".to_string()]
        );
        assert_eq!(
            program("awk '{ if ((a) && ((b))) print }' f"),
            vec!["awk".to_string()]
        );
        assert_eq!(
            program("echo $((1+2)) ${A} ${B:-{x}}"),
            vec!["echo".to_string()]
        );
        // A backslash-escaped quote must not desync the quote state and swallow the rest.
        assert!(!too_deep("echo it\\'s ( fine )"));
        assert_eq!(
            program("echo it\\'s fine && ( cd d && make )"),
            vec!["echo".to_string(), "cd".into(), "make".into()]
        );
        // A heredoc that writes braced source code is the realistic near-miss: the guard
        // scans the body as raw text, but balanced brackets never accumulate depth.
        let heredoc = "cat > src/x.rs <<'EOF'\nfn main() {\n    if a {\n        for b in c {\n            match d {\n                E::F(g) => h(i(j)),\n                _ => {}\n            }\n        }\n    }\n}\nEOF\ncargo build";
        assert!(!too_deep(heredoc));
        assert_eq!(program(heredoc), vec!["cat".to_string(), "cargo".into()]);

        // Matched nesting a few levels deep is normal shell and stays parsed.
        let nested = format!("{}echo hi;{}", "{ ".repeat(8), " };".repeat(8));
        assert_eq!(program(&nested), vec!["echo".to_string()]);
    }
}
