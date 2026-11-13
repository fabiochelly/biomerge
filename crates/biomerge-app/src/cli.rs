//! The command line: two paths to compare, or the four files of a three-way merge, and what
//! git's difftool and mergetool need around them.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::instance::{MergeRequest, Request};

pub const HELP: &str = "\
Usage : biomerge [OPTIONS] [GAUCHE DROITE]
        biomerge [OPTIONS] CHEMIN
        biomerge [OPTIONS] --head FICHIER
        biomerge --merge [OPTIONS] BASE LOCAL DISTANT RÉSULTAT

Compare deux fichiers ou deux dossiers. Sans chemins, ouvre la fenêtre vide.
Si BioMerge est déjà ouvert, la comparaison s'ouvre dans un nouvel onglet de
la fenêtre existante.

Avec un seul CHEMIN : un fichier où Git a laissé des marqueurs de conflit
s'ouvre en fusion à 3 voies, le fichier lui-même recevant le résultat (avec
--wait, le code de sortie suit alors la règle de --merge) ; sinon, la fenêtre
« Nouvelle comparaison » s'ouvre avec CHEMIN à gauche, en attendant la droite.

Avec --head, compare la version de FICHIER au dernier commit Git (à gauche,
en lecture seule) au fichier de travail (à droite), comme le menu « Comparer
avec HEAD ».

Avec --merge, fusionne à 3 voies : LOCAL à gauche, DISTANT à droite, et au
centre le résultat, écrit dans RÉSULTAT à l'enregistrement. BASE est
l'ancêtre commun. Le code de sortie vaut 0 seulement si le résultat a été
enregistré sans conflit non résolu.

Options :
  -m, --merge            Fusion à 3 voies des quatre chemins (implique --wait)
      --head FICHIER     Compare FICHIER à sa version du dernier commit Git
  -L, --label NOM        Nom affiché à la place du fichier ; la première fois
                         pour la gauche, la seconde pour la droite (avec
                         --merge : la base, puis la gauche, puis la droite)
      --label-left NOM   Nom affiché pour la gauche
      --label-right NOM  Nom affiché pour la droite
      --wait             Attend que l'onglet soit fermé avant de rendre la main
                         (nécessaire pour git difftool)
      --new-window       Ouvre une nouvelle fenêtre même si BioMerge tourne déjà
  -u                     Sans effet (compatibilité Subversion, qui le passe
                         à la commande de --diff-cmd)
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

Avec Subversion :
  svn diff --diff-cmd biomerge
                            svn appelle biomerge -u -L … -L … GAUCHE DROITE pour
                            chaque fichier modifié ; -u est sans effet
  svn diff --diff-cmd biomerge -x --wait
                            -x remplace le -u : un onglet à la fois, chacun
                            attendu avant que svn efface ses fichiers temporaires
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
    let mut head: Option<PathBuf> = None;
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
            Some((name, value)) if name.starts_with("--") => (name, Some(OsString::from(value))),
            _ => (flag, None),
        };
        let mut value = |missing: String| -> Result<OsString, String> {
            inline.clone().or_else(|| iter.next()).ok_or(missing)
        };
        let mut label = |what: &str| value(format!("{what} attend un nom")).map(|v| v.to_string_lossy().into_owned());
        match name {
            "--" => only_paths = true,
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--wait" => args.request.wait = true,
            "-m" | "--merge" => merge = true,
            "--new-window" => args.new_window = true,
            // Subversion's `--diff-cmd` asks for a unified diff, which means nothing here.
            "-u" => {}
            "--label-left" => args.request.labels[0] = Some(label("--label-left")?),
            "--label-right" => args.request.labels[1] = Some(label("--label-right")?),
            // Which input a label names depends on --merge, which may come after.
            "-L" | "--label" => short_labels.push(label(name)?),
            "--head" => head = Some(PathBuf::from(value("--head attend un fichier".to_owned())?)),
            other => return Err(format!("option inconnue : {other}")),
        }
    }
    if let Some(head) = head {
        return parse_head(args, head, merge, paths, short_labels).map(Parsed::Run);
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
    if paths.len() == 1 {
        // A name for each side of a comparison that has only one.
        if args.request.labels.iter().any(Option::is_some) {
            return Err("les noms (-L, --label-left, --label-right) demandent deux chemins".to_owned());
        }
        args.request.single = paths.pop();
        return Ok(Parsed::Run(args));
    }
    args.request.paths = match <[PathBuf; 2]>::try_from(paths) {
        Ok(pair) => Some(pair),
        Err(paths) if paths.is_empty() => None,
        Err(paths) => return Err(format!("un ou deux chemins attendus, {} donnés", paths.len())),
    };
    Ok(Parsed::Run(args))
}

/// `--head FILE`: the file against its committed version, which names both sides itself.
fn parse_head(mut args: Args, head: PathBuf, merge: bool, paths: Vec<PathBuf>, labels: Vec<String>) -> Result<Args, String> {
    if merge {
        return Err("--head et --merge ne vont pas ensemble".to_owned());
    }
    if !paths.is_empty() {
        return Err("--head compare un seul fichier, sans autre chemin".to_owned());
    }
    if !labels.is_empty() || args.request.labels.iter().any(Option::is_some) {
        return Err("--head nomme lui-même ses deux côtés : pas de -L, --label-left ni --label-right".to_owned());
    }
    args.request.head = Some(head);
    Ok(args)
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
        assert_eq!(run(&["a", "b", "c"]).unwrap_err(), "un ou deux chemins attendus, 3 donnés");
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

    #[test]
    fn the_line_subversion_passes_to_its_diff_command_is_understood() {
        let args = run(&["-u", "-L", "x.c\t(révision 12)", "-L", "x.c\t(copie de travail)", "/repo/.svn/tmp/x.c.tmp", "/repo/x.c"]).unwrap();
        assert_eq!(labels(&args), [Some("x.c\t(révision 12)"), Some("x.c\t(copie de travail)")]);
        assert_eq!(args.request.paths, Some([PathBuf::from("/repo/.svn/tmp/x.c.tmp"), PathBuf::from("/repo/x.c")]));
        assert!(!args.request.wait);
        // `-x --wait` takes the place of the -u.
        assert!(run(&["--wait", "-L", "a", "-L", "b", "x", "y"]).unwrap().request.wait);
        // Only -u: other options of diff stay unknown.
        assert_eq!(run(&["-w", "x", "y"]).unwrap_err(), "option inconnue : -w");
        assert_eq!(run(&["-uw", "x", "y"]).unwrap_err(), "option inconnue : -uw");
    }

    #[test]
    fn head_takes_one_file_after_a_space_or_an_equals_sign() {
        let args = run(&["--head", "src/x.rs", "--wait", "--new-window"]).unwrap();
        assert_eq!(args.request.head, Some(PathBuf::from("src/x.rs")));
        assert!(args.request.wait && args.new_window);
        assert_eq!(args.request.paths, None);
        assert_eq!(args.request.single, None);
        assert_eq!(run(&["--head=-odd.rs"]).unwrap().request.head, Some(PathBuf::from("-odd.rs")));
    }

    #[test]
    fn head_goes_with_neither_a_merge_nor_other_paths_nor_labels() {
        assert_eq!(run(&["--head"]).unwrap_err(), "--head attend un fichier");
        assert_eq!(run(&["--head", "a", "--merge", "b", "c", "d", "e"]).unwrap_err(), "--head et --merge ne vont pas ensemble");
        assert_eq!(run(&["--head", "a", "b"]).unwrap_err(), "--head compare un seul fichier, sans autre chemin");
        assert_eq!(run(&["b", "--head", "a"]).unwrap_err(), "--head compare un seul fichier, sans autre chemin");
        assert!(run(&["-L", "x", "--head", "a"]).unwrap_err().starts_with("--head nomme lui-même"));
        assert!(run(&["--label-right=x", "--head", "a"]).is_err());
    }

    #[test]
    fn a_single_path_is_handed_to_the_window_as_it_is() {
        let args = run(&["notes.txt"]).unwrap();
        assert_eq!(args.request.single, Some(PathBuf::from("notes.txt")));
        assert_eq!((args.request.paths, args.request.head, args.request.merge), (None, None, None));
        assert!(!args.request.wait);
        let args = run(&["--wait", "--", "-conflit.rs"]).unwrap();
        assert_eq!(args.request.single, Some(PathBuf::from("-conflit.rs")));
        assert!(args.request.wait);
        assert_eq!(run(&["-L", "x", "notes.txt"]).unwrap_err(), "les noms (-L, --label-left, --label-right) demandent deux chemins");
        assert!(run(&["--merge", "notes.txt"]).is_err(), "a merge still needs its four paths");
    }

    #[test]
    fn the_help_documents_every_new_form() {
        for needle in ["biomerge [OPTIONS] CHEMIN", "--head FICHIER", "compatibilité Subversion", "Avec git, pour les fusions", "Avec git, pour les comparaisons"] {
            assert!(HELP.contains(needle), "{needle}");
        }
    }
}
