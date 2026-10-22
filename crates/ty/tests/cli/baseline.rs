use std::fs;

use insta::assert_snapshot;
use insta_cmd::assert_cmd_snapshot;

use crate::CliTest;

#[test]
fn update_and_use_configured_baseline() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "pyproject.toml",
            r"
            [tool.ty]
            baseline = 'baseline.json'
            ",
        ),
        (
            "test.py",
            r#"
            from typing_extensions import reveal_type

            x: int = "wrong"
            reveal_type(x)
            "#,
        ),
    ])?;

    // `reveal_type` is not a lint, so it remains visible when the assignment error is baselined.
    assert_cmd_snapshot!(case.command().arg("--update-baseline"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    info[revealed-type]: Revealed type
     --> test.py:5:13
      |
    5 | reveal_type(x)
      |             ^ `int`

    Found 1 diagnostic
    Updated baseline `<temp_dir>/baseline.json` with 1 diagnostic.

    ----- stderr -----
    ");

    let serialized = fs::read_to_string(case.root().join("baseline.json"))?;
    assert_snapshot!(serialized, @r#"
    {
      "version": 0,
      "files": {
        "test.py": [
          {
            "rule": "invalid-assignment",
            "precedingHash": "33af5dcbb4264a903ea8237661d2e9d2",
            "followingHash": "9e9c1756bed4a360466ffdead18fa6a2"
          }
        ]
      }
    }
    "#);

    assert_cmd_snapshot!(case.command(), @"
    success: true
    exit_code: 0
    ----- stdout -----
    info[revealed-type]: Revealed type
     --> test.py:5:13
      |
    5 | reveal_type(x)
      |             ^ `int`

    Found 1 diagnostic

    ----- stderr -----
    ");

    case.write_file(
        "test.py",
        r#"
        from typing_extensions import reveal_type

        x: int = "wrong"
        y: int = "new"
        reveal_type(x)
        "#,
    )?;
    assert_cmd_snapshot!(case.command().arg("--output-format=concise"), @r#"
    success: false
    exit_code: 1
    ----- stdout -----
    test.py:5:10: error[invalid-assignment] Object of type `Literal["new"]` is not assignable to `int`
    test.py:6:13: info[revealed-type] Revealed type: `int`
    Found 2 diagnostics

    ----- stderr -----
    "#);

    assert_cmd_snapshot!(case.command().arg("--update-baseline"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    info[revealed-type]: Revealed type
     --> test.py:6:13
      |
    6 | reveal_type(x)
      |             ^ `int`

    Found 1 diagnostic
    Updated baseline `<temp_dir>/baseline.json` with 2 diagnostics.

    ----- stderr -----
    ");
    Ok(())
}

#[test]
fn update_baseline_is_idempotent() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "test.py",
        r"
        x: int = 'wrong'
        ",
    )?;
    assert_cmd_snapshot!(case.command().args(["--baseline", "baseline.json", "--update-baseline"]), @"
    success: true
    exit_code: 0
    ----- stdout -----
    Updated baseline `<temp_dir>/baseline.json` with 1 diagnostic.

    ----- stderr -----
    ");
    let baseline = fs::read_to_string(case.root().join("baseline.json"))?;

    assert_cmd_snapshot!(case.command().args(["--baseline", "baseline.json", "--update-baseline"]), @"
    success: true
    exit_code: 0
    ----- stdout -----
    Updated baseline `<temp_dir>/baseline.json` with 1 diagnostic.

    ----- stderr -----
    ");
    assert_eq!(
        fs::read_to_string(case.root().join("baseline.json"))?,
        baseline
    );
    Ok(())
}

#[test]
fn ignore_comment_replaces_baseline_entry() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "pyproject.toml",
            r"
            [tool.ty]
            baseline = 'baseline.json'

            [tool.ty.rules]
            unused-ignore-comment = 'error'
            ",
        ),
        (
            "test.py",
            r"
            x: int = 'wrong'
            ",
        ),
    ])?;

    assert_cmd_snapshot!(case.command().arg("--update-baseline"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    Updated baseline `<temp_dir>/baseline.json` with 1 diagnostic.

    ----- stderr -----
    ");

    case.write_file(
        "test.py",
        r"
        x: int = 'wrong'  # ty: ignore[invalid-assignment]
        ",
    )?;
    assert_cmd_snapshot!(case.command(), @"
    success: true
    exit_code: 0
    ----- stdout -----
    All checks passed!

    ----- stderr -----
    ");

    assert_cmd_snapshot!(case.command().arg("--update-baseline"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    Updated baseline `<temp_dir>/baseline.json` with 0 diagnostics.

    ----- stderr -----
    ");
    assert_snapshot!(fs::read_to_string(case.root().join("baseline.json"))?, @r#"
    {
      "version": 0,
      "files": {}
    }
    "#);
    Ok(())
}

#[test]
fn cli_baseline_path() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "test.py",
        r"
        x: int = 'wrong'
        ",
    )?;

    assert_cmd_snapshot!(case.command().args(["--baseline", "state/baseline.json", "--update-baseline"]), @"
    success: true
    exit_code: 0
    ----- stdout -----
    Updated baseline `<temp_dir>/state/baseline.json` with 1 diagnostic.

    ----- stderr -----
    ");
    assert!(case.root().join("state/baseline.json").is_file());

    assert_cmd_snapshot!(case.command().args(["--baseline", "state/baseline.json"]), @"
    success: true
    exit_code: 0
    ----- stdout -----
    All checks passed!

    ----- stderr -----
    ");
    Ok(())
}

#[test]
fn update_baseline_requires_path() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "test.py",
        r"
        x: int = 'wrong'
        ",
    )?;
    assert_cmd_snapshot!(case.command().arg("--update-baseline"), @"
    success: false
    exit_code: 2
    ----- stdout -----

    ----- stderr -----
    ty failed
      Cause: `--update-baseline` requires a baseline path from `--baseline` or configuration
    ");
    Ok(())
}

#[test]
fn update_baseline_conflicts_with_fix() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "test.py",
        r"
        x: int = 'wrong'
        ",
    )?;
    assert_cmd_snapshot!(case.command().args(["--baseline", "baseline.json", "--update-baseline", "--fix"]), @"
    success: false
    exit_code: 2
    ----- stdout -----

    ----- stderr -----
    error: the argument '--update-baseline' cannot be used with '--fix'

    Usage: ty check --baseline <PATH> --update-baseline [PATH]...

    For more information, try '--help'.
    ");
    Ok(())
}

#[test]
fn update_baseline_does_not_suppress_syntax_errors() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "test.py",
        r"
        x: int = 'wrong'

        if:
        ",
    )?;
    assert_cmd_snapshot!(case.command().args(["--baseline", "baseline.json", "--update-baseline"]), @"
    success: false
    exit_code: 1
    ----- stdout -----
    error[invalid-syntax]: Expected an expression
     --> test.py:4:3
      |
    4 | if:
      |   ^

    error[invalid-syntax]: Expected an indented block after `if` statement
     --> test.py:4:4
      |
    4 | if:
      |    ^

    Found 2 diagnostics
    Updated baseline `<temp_dir>/baseline.json` with 1 diagnostic.

    ----- stderr -----
    ");

    let baseline = fs::read_to_string(case.root().join("baseline.json"))?;
    assert_snapshot!(baseline, @r#"
    {
      "version": 0,
      "files": {
        "test.py": [
          {
            "rule": "invalid-assignment",
            "precedingHash": "d35e8dd0ee85db3b3ca8d24dba7df1f5",
            "followingHash": "cdf7576f3443c3935b0f95a430388e2e"
          }
        ]
      }
    }
    "#);
    Ok(())
}

#[test]
fn invalid_baseline_is_reported_once() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "pyproject.toml",
            r"
            [tool.ty]
            baseline = 'baseline.json'
            ",
        ),
        ("baseline.json", "{ not json"),
        (
            "test.py",
            r"
            x: int = 'wrong'
            ",
        ),
    ])?;

    assert_cmd_snapshot!(case.command().arg("--output-format=concise"), @r#"
    success: false
    exit_code: 1
    ----- stdout -----
    baseline.json:1:1: error[invalid-baseline] Failed to parse baseline `<temp_dir>/baseline.json`
    test.py:2:10: error[invalid-assignment] Object of type `Literal["wrong"]` is not assignable to `int`
    Found 2 diagnostics

    ----- stderr -----
    "#);

    // Update mode intentionally ignores the invalid old baseline and replaces it.
    assert_cmd_snapshot!(case.command().arg("--update-baseline"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    Updated baseline `<temp_dir>/baseline.json` with 1 diagnostic.

    ----- stderr -----
    ");
    let baseline = fs::read_to_string(case.root().join("baseline.json"))?;
    assert_snapshot!(baseline, @r#"
    {
      "version": 0,
      "files": {
        "test.py": [
          {
            "rule": "invalid-assignment",
            "precedingHash": "d35e8dd0ee85db3b3ca8d24dba7df1f5",
            "followingHash": "12970f0291d38ddad0189c30e27ca443"
          }
        ]
      }
    }
    "#);
    Ok(())
}

#[test]
fn baseline_serialization_sorts_files_by_path() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "b.py",
            r"
            b: int = 'b'
            ",
        ),
        (
            "a.py",
            r"
            a: int = 'a'
            ",
        ),
    ])?;
    assert_cmd_snapshot!(case.command().args(["--baseline", "baseline.json", "--update-baseline"]), @"
    success: true
    exit_code: 0
    ----- stdout -----
    Updated baseline `<temp_dir>/baseline.json` with 2 diagnostics.

    ----- stderr -----
    ");

    let baseline = fs::read_to_string(case.root().join("baseline.json"))?;
    assert_snapshot!(baseline, @r#"
    {
      "version": 0,
      "files": {
        "a.py": [
          {
            "rule": "invalid-assignment",
            "precedingHash": "9053829516334961569e2352092c039d",
            "followingHash": "cfd18b7b7e8b411ea044878c0e8b6771"
          }
        ],
        "b.py": [
          {
            "rule": "invalid-assignment",
            "precedingHash": "37724c192c478a1109c6c2221df1023d",
            "followingHash": "799da6a5d8e28b745c49494de187b60a"
          }
        ]
      }
    }
    "#);
    Ok(())
}
