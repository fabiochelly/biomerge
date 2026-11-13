//! The command line: two paths to compare, or the four files of a three-way merge, and what
//! git's difftool and mergetool need around them.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::instance::{MergeRequest, Request};

pub const HELP: &str = "\
Usage : biomerge [OPTIONS] [GAUCHE DROITE]
        biomerge --merge [OPTIONS] BASE LOCAL DISTANT RÉSULTAT

Compare deux fichiers ou deux dossiers. Sans chemins, ouvre la fenêtre vide.
Si BioMerge est déjà ouvert, la comparaison s'ouvre dans un nouvel onglet de
la fenêtre existante.

Avec --merge, fusionne à 3 voies : LOCAL à gauche, DISTANT à droite, et au
centre le résultat, écrit dans RÉSULTAT à l'enregistrement. BASE est
l'ancêtre commun. Le code de sortie vaut 0 seulement si le résultat a été
enregistré sans conflit non résolu.

Options :
  -m, --merge            Fusion à 3 voies des quatre chemins (implique --wait)
  -L, --label NOM        Nom affiché à la place du fichier ; la première fois
                         pour la gauche, la seconde pour la droite (avec
                         --merge : la base, puis la gauche, puis la droite)
      --label-left NOM   Nom affiché pour la gauche
      --label-right NOM  Nom affiché pour la droite
      --wait             Attend que l'onglet soit fermé avant de rendre la main
                         (nécessaire pour git difftool)
      --new-window       Ouvre une nouvelle fenêtre même si BioMerge tourne déjà
  -h, --help             Affiche cette aide
  -V, --version          Affiche la version

Avec git, pour les fusions :
  git config --global merge.tool biomerge
  git config --global mergetool.biomerge.cmd \\
    'biomerge --wait --merge -L \"$MERGED (base)\" -L \"$MERGED (local)\" -L \"$MERGED (distant)\" \"$BASE\" \"$LOCAL\" \"$REMOTE\" \"$MERGED\"'
  git config --global mergetool.biomerge.trustExitCode true

  git mergetool             résout chaque fichier en conflit, un onglet à la fois ;
                            fermer l'onglet sans enregistrer, ou enregistrer avec
                            des marqueurs, laisse le fichier en conflit

Avec git, pour les comparaisons :
  git config --global diff.tool biomerge
  git config --global difftool.prompt false
  git config --global difftool.biomerge.cmd \\
    'biomerge --wait -L \"$MERGED (base)\" -L \"$MERGED\" \"$LOCAL\" \"$REMOTE\"'

  git difftool              compare chaque fichier modifié, un onglet à la fois
  git difftool --dir-diff   compare deux dossiers temporaires en un seul onglet ;
                            git y place des liens vers les fichiers de travail
                            (affichés de type différent), --no-symlinks met des
                            copies comparées octet par octet, que git recopie
                            dans l'arbre de travail si elles sont modifiées
";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub request: Request,
    pub new_window: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Args),
    Help,
    Version,
}

/// Reads the arguments that follow the program name. The error is a French sentence to print
/// before the usage.
pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Parsed, String> {
    let mut args = Args::default();
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut short_labels: Vec<String> = Vec::new();
    let mut merge = false;
    let mut only_paths = false;
    let mut iter = arguments.into_iter();
    while let Some(argument) = iter.next() {
        let text = argument.to_str().map(str::to_owned);
        let flag = text.as_deref().filter(|t| !only_paths && t.starts_with('-') && *t != "-");
        let Some(flag) = flag else {
            paths.push(PathBuf::from(argument));
            continue;
        };
        // `--label-left=NAME` as well as `--label-left NAME`.
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_owned())),
            _ => (flag, None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            match inline.clone() {
                Some(value) => Ok(value),
                None => iter
                    .next()
                    .map(|v| v.to_string_lossy().into_owned())
                    .ok_or_else(|| format!("{what} attend un nom")),
            }
        };
        match name {
            "--" => only_paths = true,
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--wait" => args.request.wait = true,
            "-m" | "--merge" => merge = true,
            "--new-window" => args.new_window = true,
            "--label-left" => args.request.labels[0] = Some(value("--label-left")?),
            "--label-right" => args.request.labels[1] = Some(value("--label-right")?),
            // Which input a label names depends on --merge, which may come after.
            "-L" | "--label" => short_labels.push(value(name)?),
            other => return Err(format!("option inconnue : {other}")),
        }
    }
    if merge {
        return parse_merge(args, paths, short_labels).map(Parsed::Run);
    }
    if short_labels.len() > 2 {
        return Err("-L ne peut servir que deux fois".to_owned());
    }
    for (slot, label) in short_labels.into_iter().enumerate() {
        args.request.labels[slot] = Some(label);
    }
    args.request.paths = match <[PathBuf; 2]>::try_from(paths) {
        Ok(pair) => Some(pair),
        Err(paths) if paths.is_empty() => None,
        Err(paths) if paths.len() == 1 => return Err("deux chemins attendus, un seul donné".to_owned()),
        Err(paths) => return Err(format!("deux chemins attendus, {} donnés", paths.len())),
    };
    Ok(Parsed::Run(args))
}

/// `--merge BASE LOCAL REMOTE MERGED`. A merge tool must not return before the user is done,
/// as git reads the result then: waiting goes with it.
fn parse_merge(mut args: Args, paths: Vec<PathBuf>, labels: Vec<String>) -> Result<Args, String> {
    if labels.len() > 3 {
        return Err("-L ne peut servir que trois fois avec --merge".to_owned());
    }
    let [base, local, remote, merged] = <[PathBuf; 4]>::try_from(paths)
        .map_err(|paths| format!("--merge attend quatre chemins (BASE LOCAL DISTANT RÉSULTAT), {} donné{}", paths.len(), if paths.len() > 1 { "s" } else { "" }))?;
    let mut request = MergeRequest { base, local, remote, merged, labels: Default::default() };
    for (slot, label) in labels.into_iter().enumerate() {
        request.labels[slot] = Some(label);
    }
    args.request.merge = Some(request);
    args.request.wait = true;
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Args, String> {
        match parse(args.iter().map(OsString::from))? {
            Parsed::Run(args) => Ok(args),
            other => panic!("{other:?}"),
        }
    }

    fn labels(args: &Args) -> [Option<&str>; 2] {
        [args.request.labels[0].as_deref(), args.request.labels[1].as_deref()]
    }

    #[test]
    fn two_paths_alone_are_compared_without_labels() {
        let args = run(&["a.rs", "b.rs"]).unwrap();
        assert_eq!(args.request.paths, Some([PathBuf::from("a.rs"), PathBuf::from("b.rs")]));
        assert_eq!(labels(&args), [None, None]);
        assert!(!args.request.wait && !args.new_window);
        assert_eq!(run(&[]).unwrap().request.paths, None);
    }

    #[test]
    fn short_labels_name_the_left_then_the_right() {
        let args = run(&["--wait", "-L", "src/x.rs (base)", "-L", "src/x.rs", "/tmp/git-blob-1/x.rs", "src/x.rs"]).unwrap();
        assert_eq!(labels(&args), [Some("src/x.rs (base)"), Some("src/x.rs")]);
        assert!(args.request.wait);
        assert_eq!(args.request.paths.unwrap()[0], PathBuf::from("/tmp/git-blob-1/x.rs"));
        assert!(run(&["-L", "a", "-L", "b", "-L", "c", "x", "y"]).is_err());
    }

    #[test]
    fn long_labels_take_a_value_after_a_space_or_an_equals_sign() {
        let args = run(&["--label-right=droite", "--label-left", "gauche", "--new-window", "x", "y"]).unwrap();
        assert_eq!(labels(&args), [Some("gauche"), Some("droite")]);
        assert!(args.new_window);
    }

    #[test]
    fn mistakes_are_reported_in_french() {
        assert_eq!(run(&["a"]).unwrap_err(), "deux chemins attendus, un seul donné");
        assert_eq!(run(&["a", "b", "c"]).unwrap_err(), "deux chemins attendus, 3 donnés");
        assert_eq!(run(&["--label-left"]).unwrap_err(), "--label-left attend un nom");
        assert_eq!(run(&["--frobnicate", "a", "b"]).unwrap_err(), "option inconnue : --frobnicate");
        assert_eq!(parse([OsString::from("--help")]), Ok(Parsed::Help));
    }

    #[test]
    fn the_merge_form_takes_four_paths_and_three_labels_and_always_waits() {
        let args = run(&["--merge", "-L", "x.rs (base)", "-L", "x.rs (local)", "-L", "x.rs (distant)", "./x_BASE_1.rs", "./x_LOCAL_1.rs", "./x_REMOTE_1.rs", "x.rs"]).unwrap();
        let merge = args.request.merge.expect("a merge");
        assert_eq!(
            [&merge.base, &merge.local, &merge.remote, &merge.merged].map(|p| p.to_str().unwrap()),
            ["./x_BASE_1.rs", "./x_LOCAL_1.rs", "./x_REMOTE_1.rs", "x.rs"]
        );
        assert_eq!(merge.labels.each_ref().map(Option::as_deref), [Some("x.rs (base)"), Some("x.rs (local)"), Some("x.rs (distant)")]);
        assert!(args.request.wait, "a merge tool always waits");
        assert_eq!(args.request.paths, None);
        // The flag may come last, after the labels and the paths.
        let args = run(&["-L", "b", "a", "b", "c", "d", "-m"]).unwrap();
        assert_eq!(args.request.merge.unwrap().labels[0].as_deref(), Some("b"));
    }

    #[test]
    fn a_merge_without_its_four_paths_is_refused() {
        assert_eq!(run(&["--merge", "a", "b", "c"]).unwrap_err(), "--merge attend quatre chemins (BASE LOCAL DISTANT RÉSULTAT), 3 donnés");
        assert_eq!(run(&["-m"]).unwrap_err(), "--merge attend quatre chemins (BASE LOCAL DISTANT RÉSULTAT), 0 donné");
        assert!(run(&["-m", "-L", "1", "-L", "2", "-L", "3", "-L", "4", "a", "b", "c", "d"]).is_err());
        assert!(run(&["-L", "1", "-L", "2", "-L", "3", "a", "b"]).is_err(), "three labels need --merge");
    }

    #[test]
    fn a_double_dash_lets_paths_start_with_a_dash() {
        let args = run(&["--", "-odd", "b"]).unwrap();
        assert_eq!(args.request.paths, Some([PathBuf::from("-odd"), PathBuf::from("b")]));
    }
}
