/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::io;
use std::path::Path;

use crate::extract_from_outputs;
use crate::runtime::Term;
use crate::runtime::ZshRuntime;

pub(crate) fn run_zsh(script: &str, input: &str, tempdir: &Path) -> io::Result<Vec<String>> {
    let home = tempdir;

    // Copy and paste of `ZshRuntime::new` which works around a zsh bug in which completions are not
    // autoloaded completely
    let config_path = home.join(".zshenv");
    let config = "\
fpath=($fpath $ZDOTDIR/zsh)
autoload -U +X compinit && compinit -u # bypass compaudit security checking
precmd_functions=\"\"  # avoid the prompt being overwritten
PS1='%% '
PROMPT='%% '
_buck2 >/dev/null 2>/dev/null ; # Force the completion to be loaded
";
    std::fs::write(config_path, config)?;

    let mut r = ZshRuntime::with_home(home.to_owned())?;
    r.register("buck2", script)?;

    extract_from_outputs(
        input,
        [
            r.complete(&format!("{}\t", input), &Term::new()),
            r.complete(&format!("{}\t\t", input), &Term::new()),
        ],
    )
}