//! The PowerShell script reader: how a script is split into statements and
//! words, which statement is a command, what `Start-Process` starts, and how
//! the options before the script are taken.

use super::*;

fn strs(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

fn words(statement: &str) -> Vec<(String, bool, bool)> {
    ps_words(statement)
        .into_iter()
        .map(|w| (w.text, w.quoted, w.expands))
        .collect()
}

fn command(statement: &str) -> Result<Option<Vec<String>>, String> {
    ps_command(&ps_words(statement))
}

#[test]
fn a_script_splits_into_statements_outside_quotes() {
    assert_eq!(
        ps_statements("a;b|c\nd\re{f}g(h)i"),
        strs(&["a", "b", "c", "d", "e", "f", "g", "h", "i"])
    );
    assert_eq!(ps_statements("a || b"), strs(&["a", "b"]));
    assert_eq!(ps_statements("a && b"), strs(&["a", "b"]));
    assert_eq!(
        ps_statements("a | b || c && d"),
        strs(&["a", "b", "c", "d"])
    );
    // One `&` is the call operator, not a separator.
    assert_eq!(ps_statements("a & b"), strs(&["a & b"]));
    assert_eq!(ps_statements("& b"), strs(&["& b"]));
    assert_eq!(ps_statements("a &"), strs(&["a &"]));
    assert_eq!(ps_statements("a |"), strs(&["a"]));
    assert_eq!(ps_statements("a ||"), strs(&["a"]));
    assert_eq!(ps_statements("a &&"), strs(&["a"]));
    assert_eq!(ps_statements(""), Vec::<String>::new());
    assert_eq!(ps_statements(" ; ; "), Vec::<String>::new());
    // Separators inside single or double quotes stay.
    assert_eq!(ps_statements("'a;b'; c"), strs(&["'a;b'", "c"]));
    assert_eq!(ps_statements("\"a;b\"; c"), strs(&["\"a;b\"", "c"]));
    assert_eq!(ps_statements("'a|b' | c"), strs(&["'a|b'", "c"]));
    assert_eq!(ps_statements("'a && b'; c"), strs(&["'a && b'", "c"]));
    assert_eq!(ps_statements("\"a && b\"; c"), strs(&["\"a && b\"", "c"]));
    assert_eq!(ps_statements("'a\nb'; c"), strs(&["'a\nb'", "c"]));
    assert_eq!(ps_statements("\"a{b}(c)\"; d"), strs(&["\"a{b}(c)\"", "d"]));
    // One kind of quote inside the other does not end it.
    assert_eq!(ps_statements("\"it's\"; x"), strs(&["\"it's\"", "x"]));
    assert_eq!(
        ps_statements("'say \"hi\"'; y"),
        strs(&["'say \"hi\"'", "y"])
    );
    assert_eq!(
        ps_statements("\"it's; here\"; x"),
        strs(&["\"it's; here\"", "x"])
    );
    assert_eq!(
        ps_statements("'say \"hi; there\"'; y"),
        strs(&["'say \"hi; there\"'", "y"])
    );
    // An unpaired quote of one kind inside the other kind changes nothing.
    assert_eq!(ps_statements("'a \" b'; c"), strs(&["'a \" b'", "c"]));
    assert_eq!(ps_statements("\"a ' b\"; c"), strs(&["\"a ' b\"", "c"]));
    // A quote closes, and the text after it splits again.
    assert_eq!(ps_statements("'a'; b"), strs(&["'a'", "b"]));
    assert_eq!(ps_statements("\"a\"; b"), strs(&["\"a\"", "b"]));
    assert_eq!(ps_statements("'a' && b"), strs(&["'a'", "b"]));
}

#[test]
fn a_statement_splits_into_words_with_their_quoting() {
    let w = |t: &str, q: bool, e: bool| (t.to_owned(), q, e);
    assert_eq!(
        words("curl 'b c' \"d e\" f"),
        vec![
            w("curl", false, false),
            w("b c", true, false),
            w("d e", true, false),
            w("f", false, false)
        ]
    );
    assert_eq!(
        words("a\tb"),
        vec![w("a", false, false), w("b", false, false)]
    );
    assert_eq!(
        words("  a   b  "),
        vec![w("a", false, false), w("b", false, false)]
    );
    assert_eq!(words("$x"), vec![w("$x", false, true)]);
    assert_eq!(words("'$x'"), vec![w("$x", true, false)]);
    assert_eq!(words("\"$x\""), vec![w("$x", true, true)]);
    assert_eq!(words("a$b"), vec![w("a$b", false, true)]);
    assert_eq!(words("''"), vec![w("", true, false)]);
    assert_eq!(words("\"\""), vec![w("", true, false)]);
    assert_eq!(words("'a''b'"), vec![w("ab", true, false)]);
    assert_eq!(words("'a'b"), vec![w("ab", true, false)]);
    assert_eq!(words(""), Vec::new());
    assert_eq!(words("   "), Vec::new());
    // One kind of quote inside the other does not end it.
    assert_eq!(
        words("\"it's here\" x"),
        vec![w("it's here", true, false), w("x", false, false)]
    );
    assert_eq!(
        words("'say \"hi there\"' x"),
        vec![w("say \"hi there\"", true, false), w("x", false, false)]
    );
    assert_eq!(words("\"a 'b c' d\""), vec![w("a 'b c' d", true, false)]);
    assert_eq!(words("'a $b'"), vec![w("a $b", true, false)]);
    assert_eq!(words("\"a $b\""), vec![w("a $b", true, true)]);
    // Spaces and tabs inside quotes stay.
    assert_eq!(words("'a\tb'"), vec![w("a\tb", true, false)]);
    assert_eq!(words("\"a\tb\""), vec![w("a\tb", true, false)]);
}

#[test]
fn a_statement_is_a_command_an_expression_or_unreadable() {
    let some = |w: &[&str]| Ok(Some(strs(w)));
    assert_eq!(command("curl x"), some(&["curl", "x"]));
    assert_eq!(command("curl 'a b'"), some(&["curl", "a b"]));
    // The call operator names the program in its next word.
    assert_eq!(
        command("& 'C:\\tools\\curl.exe' x"),
        some(&["C:\\tools\\curl.exe", "x"])
    );
    assert_eq!(command("& curl x"), some(&["curl", "x"]));
    assert!(command("& $x").is_err());
    assert!(command("&").is_err());
    assert!(command("& \"$x\"").is_err());
    assert!(command("& a$b").is_err());
    assert!(command("& '$x'").is_err());
    // Dot-sourcing a script; a lone dot is no command.
    assert!(command(". x.ps1").unwrap_err().contains("dot-sources"));
    assert_eq!(command("."), Ok(None));
    // A variable is an expression; an assignment's right side runs.
    assert_eq!(command("$x"), Ok(None));
    assert_eq!(command("$x = 5"), Ok(None));
    assert_eq!(command("$x = curl y"), some(&["curl", "y"]));
    assert_eq!(command("$x += curl y"), some(&["curl", "y"]));
    for op in ["-=", "*=", "/=", "??="] {
        assert_eq!(
            command(&format!("$x {op} curl y")),
            some(&["curl", "y"]),
            "{op}"
        );
    }
    assert_eq!(command("$x -eq 1"), Ok(None));
    assert_eq!(command("$x = $y"), Ok(None));
    assert_eq!(command("$x = \"=\" curl"), Ok(None));
    // A quoted word, a number, a member access and an option are no command.
    assert_eq!(command("'curl' x"), Ok(None));
    assert_eq!(command("\"curl\" x"), Ok(None));
    assert_eq!(command("5"), Ok(None));
    assert_eq!(command("5.5 x"), Ok(None));
    assert_eq!(command(".x"), Ok(None));
    assert_eq!(command("-x"), Ok(None));
    // A relative path runs a program.
    for p in [".\\tool.exe", "./tool", "..\\tool.exe", "../bin/tool"] {
        assert_eq!(command(&format!("{p} a")), some(&[p, "a"]), "{p}");
    }
    // A keyword is no command; a word with a variable names none.
    assert_eq!(command("if x"), Ok(None));
    assert_eq!(command("IF x"), Ok(None));
    assert!(
        command("a$b x")
            .unwrap_err()
            .contains("named by a variable")
    );
    assert_eq!(command(""), Ok(None));
}

#[test]
fn start_process_names_its_program_and_arguments() {
    let r = |a: &[&str]| start_process(&strs(a));
    let ok = |a: &[&str]| Ok(strs(a));
    assert_eq!(r(&["Start-Process", "curl"]), ok(&["curl"]));
    assert_eq!(
        r(&["Start-Process", "curl", "a", "b"]),
        ok(&["curl", "a", "b"])
    );
    assert_eq!(r(&["Start-Process", "-FilePath", "curl"]), ok(&["curl"]));
    assert_eq!(r(&["Start-Process", "-filepath", "curl"]), ok(&["curl"]));
    assert_eq!(r(&["Start-Process", "-f", "curl"]), ok(&["curl"]));
    assert_eq!(r(&["Start-Process", "-fil", "curl"]), ok(&["curl"]));
    assert_eq!(
        r(&["Start-Process", "-FilePath", "curl", "-ArgumentList", "a,b"]),
        ok(&["curl", "a", "b"])
    );
    assert_eq!(
        r(&["Start-Process", "-a", "x, 'y'", "curl"]),
        ok(&["curl", "x", "y"])
    );
    assert_eq!(
        r(&["Start-Process", "-arg", "x", "curl"]),
        ok(&["curl", "x"])
    );
    assert_eq!(
        r(&["Start-Process", "-ArgumentList", "a,,b", "curl"]),
        ok(&["curl", "a", "b"])
    );
    // An option that takes a value, and the ones that take none.
    assert_eq!(
        r(&["Start-Process", "-Verb", "RunAs", "-FilePath", "curl"]),
        ok(&["curl"])
    );
    assert_eq!(
        r(&["Start-Process", "-WorkingDirectory", "d", "curl", "x"]),
        ok(&["curl", "x"])
    );
    for flag in [
        "-Wait",
        "-NoNewWindow",
        "-PassThru",
        "-UseNewEnvironment",
        "-LoadUserProfile",
    ] {
        assert_eq!(
            r(&["Start-Process", flag, "-FilePath", "curl"]),
            ok(&["curl"]),
            "{flag}"
        );
        assert_eq!(
            r(&["Start-Process", flag, "curl", "x"]),
            ok(&["curl", "x"]),
            "{flag}"
        );
        assert_eq!(
            r(&["Start-Process", "-Verb", "x", flag, "curl"]),
            ok(&["curl"]),
            "{flag}"
        );
    }
    // `-F` followed by letters that are no prefix of `-FilePath` is another option.
    assert_eq!(r(&["Start-Process", "-Fx", "v", "curl"]), ok(&["curl"]));
    assert_eq!(r(&["Start-Process", "-Ax", "v", "curl"]), ok(&["curl"]));
    // No program, or one named by a variable or empty.
    assert!(r(&["Start-Process"]).unwrap_err().contains("cannot name"));
    assert!(
        r(&["Start-Process", "-Wait"])
            .unwrap_err()
            .contains("cannot name")
    );
    assert!(
        r(&["Start-Process", "-FilePath"])
            .unwrap_err()
            .contains("cannot name")
    );
    assert!(
        r(&["Start-Process", "$x"])
            .unwrap_err()
            .contains("named by a variable")
    );
    assert!(
        r(&["Start-Process", "a$b"])
            .unwrap_err()
            .contains("named by a variable")
    );
    assert!(
        r(&["Start-Process", "-FilePath", ""])
            .unwrap_err()
            .contains("named by a variable")
    );
    assert!(
        r(&["Start-Process", ""])
            .unwrap_err()
            .contains("named by a variable")
    );
}

#[test]
fn the_options_before_the_script_are_taken_by_their_kind() {
    let text = |args: &[&str]| match ps_script_text("powershell", &strs(args)) {
        Ok(s) => Ok(s.text),
        Err(found) => Err(found),
    };
    assert_eq!(
        text(&["-Command", "curl x"]).ok(),
        Some("curl x".to_owned())
    );
    assert_eq!(
        text(&["/Command", "curl x"]).ok(),
        Some("curl x".to_owned())
    );
    assert_eq!(text(&["/c", "curl x"]).ok(), Some("curl x".to_owned()));
    assert_eq!(
        text(&["-NoProfile", "-Command", "curl x"]).ok(),
        Some("curl x".to_owned())
    );
    assert_eq!(
        text(&[
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "curl x"
        ])
        .ok(),
        Some("curl x".to_owned())
    );
    assert_eq!(
        text(&[
            "-NoLogo",
            "-NoProfile",
            "-WindowStyle",
            "Hidden",
            "-Command",
            "curl",
            "x"
        ])
        .ok(),
        Some("curl x".to_owned())
    );
    // A lone `/` is a plain argument, not an option.
    assert_eq!(text(&["/", "x"]).ok(), Some("/ x".to_owned()));
    assert_eq!(text(&["curl", "x"]).ok(), Some("curl x".to_owned()));
    // An option that kGraf cannot place makes the reading unreadable.
    assert!(matches!(
        text(&["-NoProfile", "-Bogus", "-Command", "curl x"]),
        Err(found) if matches!(found.as_slice(), [Embedded::Unreadable(_)])
    ));
    assert!(text(&["-Command"]).unwrap_err().is_empty());
    assert!(matches!(
        text(&["-Command", "-"]),
        Err(found) if matches!(found.as_slice(), [Embedded::Unreadable(_)])
    ));
    assert!(matches!(
        text(&["-NoExit"]),
        Err(found) if matches!(found.as_slice(), [Embedded::Unreadable(_)])
    ));
    assert!(matches!(
        text(&["-File", "x.ps1"]),
        Err(found) if matches!(found.as_slice(), [Embedded::Unreadable(_)])
    ));
    assert!(matches!(
        text(&["-EncodedCommand"]),
        Err(found) if matches!(found.as_slice(), [Embedded::Unreadable(_)])
    ));
    assert!(matches!(
        text(&[]),
        Err(found) if matches!(found.as_slice(), [Embedded::Unreadable(_)])
    ));
    // `pwsh FILE` runs a script file.
    assert!(matches!(
        ps_script_text("pwsh", &strs(&["x.ps1"])),
        Err(found) if matches!(found.as_slice(), [Embedded::Unreadable(_)])
    ));
}

fn cs(script: &str) -> Vec<Embedded> {
    cmd_script(script, false)
}

fn argvs(found: &[Embedded]) -> Vec<Vec<String>> {
    found
        .iter()
        .filter_map(|e| match e {
            Embedded::Argv(a) => Some(a.clone()),
            _ => None,
        })
        .collect()
}

fn reasons(found: &[Embedded]) -> String {
    found
        .iter()
        .filter_map(|e| match e {
            Embedded::Unreadable(w) => Some(w.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn a_cmd_script_splits_on_operators_outside_quotes_and_carets() {
    // `&`, `&&`, `|` and `||` separate commands; one `&` after `>` does not.
    for script in [
        "a & b", "a && b", "a | b", "a || b", "a&b", "a&&b", "a|b", "a||b",
    ] {
        assert_eq!(
            argvs(&cs(script)),
            vec![strs(&["a"]), strs(&["b"])],
            "{script}"
        );
    }
    assert_eq!(argvs(&cs("a 2>&1")), vec![strs(&["a"])]);
    assert_eq!(argvs(&cs("a >&2 & b")), vec![strs(&["a"]), strs(&["b"])]);
    assert_eq!(
        argvs(&cs("a & b && c | d")),
        vec![strs(&["a"]), strs(&["b"]), strs(&["c"]), strs(&["d"])]
    );
    // Line breaks separate commands.
    assert_eq!(argvs(&cs("a\nb")), vec![strs(&["a"]), strs(&["b"])]);
    assert_eq!(argvs(&cs("a\r\nb")), vec![strs(&["a"]), strs(&["b"])]);
    // Inside quotes, nothing separates.
    assert_eq!(
        argvs(&cs("echo \"a & b\" & curl x")),
        vec![strs(&["echo", "a & b"]), strs(&["curl", "x"])]
    );
    assert_eq!(argvs(&cs("echo \"a | b\"")), vec![strs(&["echo", "a | b"])]);
    assert_eq!(argvs(&cs("echo \"a\nb\"")), vec![strs(&["echo", "a\nb"])]);
    // A caret inside quotes is plain text, so it does not hide the closing quote.
    assert_eq!(
        argvs(&cs("echo \"a^\" & curl x")),
        vec![strs(&["echo", "a^"]), strs(&["curl", "x"])]
    );
    // A caret escapes the next character outside quotes.
    assert_eq!(
        argvs(&cs("echo a ^& b")),
        vec![strs(&["echo", "a", "&", "b"])]
    );
    assert_eq!(
        argvs(&cs("echo a ^| b")),
        vec![strs(&["echo", "a", "|", "b"])]
    );
    // Empty commands run nothing.
    assert!(cs("").is_empty());
    assert!(cs(" & & ").is_empty());
}

#[test]
fn a_cmd_script_refuses_groups_clauses_and_variables_as_programs() {
    for script in [
        "(dir)", "( dir )", "@(dir)", "@ dir", "a & (b)", "(a) & b", " (a)",
    ] {
        let found = cs(script);
        assert!(
            reasons(&found).contains("parenthesized group"),
            "{script}: {found:?}"
        );
    }
    for script in ["if x", "IF x", "for %a in (b) do c", "a & if x"] {
        let found = cs(script);
        assert!(
            reasons(&found).contains("whose clauses run commands")
                || reasons(&found).contains("parenthesized group"),
            "{script}: {found:?}"
        );
    }
    for script in ["%x% a", "a & %x%", "x%y% a"] {
        let found = cs(script);
        assert!(
            reasons(&found).contains("named by a variable"),
            "{script}: {found:?}"
        );
    }
    // `!NAME!` expands only with delayed expansion.
    let delayed = cmd_script("a!b! c", true);
    assert!(
        reasons(&delayed).contains("named by a variable"),
        "{delayed:?}"
    );
    assert_eq!(
        argvs(&cmd_script("a!b! c", false)),
        vec![strs(&["a!b!", "c"])]
    );
    assert_eq!(
        argvs(&cmd_script("curl x", true)),
        vec![strs(&["curl", "x"])]
    );
    // A percent sign in an argument, not the program, is not refused here.
    assert_eq!(argvs(&cs("echo %x%")), vec![strs(&["echo", "%x%"])]);
}

#[test]
fn a_cmd_script_names_call_and_start_targets() {
    assert_eq!(argvs(&cs("call curl x")), vec![strs(&["curl", "x"])]);
    assert_eq!(argvs(&cs("CALL curl x")), vec![strs(&["curl", "x"])]);
    assert!(cs("call").is_empty());
    assert!(reasons(&cs("call :label")).contains("label of a batch file"));
    // `start` runs its program, after its switches and a window title.
    assert_eq!(
        argvs(&cs("start curl x")),
        vec![strs(&["start", "curl", "x"]), strs(&["curl", "x"])]
    );
    assert_eq!(
        argvs(&cs("start /b /wait curl x")),
        vec![
            strs(&["start", "/b", "/wait", "curl", "x"]),
            strs(&["curl", "x"])
        ]
    );
    assert_eq!(
        argvs(&cs("start \"title\" curl x")),
        vec![strs(&["start", "title", "curl", "x"]), strs(&["curl", "x"])]
    );
    assert!(reasons(&cs("start")).contains("starts a new shell"));
    assert!(reasons(&cs("start /b")).contains("starts a new shell"));
    // A built-in or a name cmd ends early is noted.
    assert!(reasons(&cs("rd\\x a")).contains("built-in `rd`"));
    assert!(reasons(&cs("a,b c")).contains("ends at"));
    assert!(reasons(&cs("curl.exe x")).is_empty());
}

#[test]
fn the_switches_before_the_command_are_plain_or_unmodeled() {
    for plain in ["/q", "/d", "/s", "/v:on", "/e:off", "/t:0a", "/Q", "/V:ON"] {
        assert!(is_plain_cmd_switch(&plain.to_ascii_lowercase()), "{plain}");
    }
    for odd in [
        "/", "/1", "/x9", "/v:", "/qx", "/v:o-n", "/v:o n", "//c", "/q/c", "q", "", "/:on",
        "/v:on:x",
    ] {
        assert!(!is_plain_cmd_switch(odd), "{odd}");
    }
    // Through the reader: an odd switch notes it; a plain one does not.
    let found = cmd(&strs(&["/q", "/c", "curl", "x"]));
    assert_eq!(found, vec![Embedded::Argv(strs(&["curl", "x"]))]);
    let found = cmd(&strs(&["/qx", "/c", "curl", "x"]));
    assert!(reasons(&found).contains("does not model how cmd reads it"));
    assert_eq!(argvs(&found), vec![strs(&["curl", "x"])]);
    // `/v:on` turns on delayed expansion.
    let found = cmd(&strs(&["/v:on", "/c", "a!b!", "c"]));
    assert!(reasons(&found).contains("named by a variable"), "{found:?}");
    let found = cmd(&strs(&["/c", "a!b!", "c"]));
    assert!(reasons(&found).contains("holds `!`"), "{found:?}");
    for lead in ["/c", "/C", "/k", "/K", "/r", "/R"] {
        let found = cmd(&strs(&[lead, "curl", "x"]));
        assert_eq!(found, vec![Embedded::Argv(strs(&["curl", "x"]))], "{lead}");
    }
}

#[test]
fn a_quoted_command_line_loses_one_pair_of_outer_quotes() {
    // The text after `/c` is one quoted string.
    let found = cmd(&strs(&["/c", "\"curl x\""]));
    assert!(reasons(&found).contains("holds `\"`"), "{found:?}");
    assert_eq!(argvs(&found), vec![strs(&["curl", "x"])]);
    // With `/s` the pair is always stripped, whatever is inside.
    let found = cmd(&strs(&["/s", "/c", "\"curl \"x y\"\""]));
    assert_eq!(argvs(&found), vec![strs(&["curl", "x y"])], "{found:?}");
    // Without `/s`, a pair with quotes inside is kept as it is.
    let found = cmd(&strs(&["/c", "\"a \"b c\" d\""]));
    assert_eq!(argvs(&found).len(), 1, "{found:?}");
    assert_ne!(argvs(&found)[0][0], "a", "{found:?}");
    // No quote, nothing to strip.
    let found = cmd(&strs(&["/c", "curl", "x"]));
    assert_eq!(found, vec![Embedded::Argv(strs(&["curl", "x"]))]);
    // An empty `/c` and no `/c`.
    assert!(reasons(&cmd(&strs(&["/c"]))).contains("empty `/c`"));
    assert!(reasons(&cmd(&strs(&["/c", " "]))).contains("empty `/c`"));
    assert!(reasons(&cmd(&strs(&["/q"]))).contains("no `/c`"));
    assert!(reasons(&cmd(&strs(&["dir"]))).contains("no `/c`"));
    assert!(reasons(&cmd(&[])).contains("no `/c`"));
}

#[test]
fn a_cmd_program_name_that_cmd_reads_differently_is_noted() {
    for program in ["a,b", "a;b", "a=b", "a/b", "dir/x"] {
        let why = cmd_program_unmodeled(program).unwrap_or_default();
        assert!(why.contains("ends at"), "{program}: {why}");
    }
    for program in [
        "cd.x", "cd\\x", "cd:x", "cd+x", "cd[x", "cd]x", "RD\\x", "echo.", "dir.exe",
    ] {
        let why = cmd_program_unmodeled(program).unwrap_or_default();
        assert!(why.contains("built-in"), "{program}: {why}");
    }
    for program in ["curl", "curl.exe", ".dir", "x.cd", "cdx.y", "", "a.b"] {
        assert_eq!(cmd_program_unmodeled(program), None, "{program}");
    }
}
