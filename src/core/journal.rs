//! Journal de l'app dans le dossier de données (`journal.txt`).
//!
//! Sous Windows, l'app n'a pas de console : sans ce fichier, les messages et
//! les plantages ne laisseraient aucune trace. Le journal reste petit :
//! au-delà de [`MAX_BYTES`], il devient `journal-ancien.txt` et repart de zéro.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// Taille au-delà de laquelle le journal est mis de côté au lancement.
pub const MAX_BYTES: u64 = 1024 * 1024;

pub fn path(root: &Path) -> PathBuf {
    root.join("journal.txt")
}

/// Ouvre le journal en ajout, en mettant de côté celui devenu trop gros.
pub fn open(root: &Path) -> std::io::Result<File> {
    fs::create_dir_all(root)?;
    let path = path(root);
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = fs::rename(&path, root.join("journal-ancien.txt"));
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_mis_de_cote_quand_trop_gros() {
        let root = std::env::temp_dir().join(format!("rip-journal-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        writeln!(open(&root).unwrap(), "premier lancement").unwrap();
        writeln!(open(&root).unwrap(), "deuxième").unwrap();
        let text = fs::read_to_string(path(&root)).unwrap();
        assert!(text.contains("premier lancement") && text.contains("deuxième"));

        fs::write(path(&root), vec![b'x'; MAX_BYTES as usize + 1]).unwrap();
        writeln!(open(&root).unwrap(), "après").unwrap();
        assert!(fs::metadata(path(&root)).unwrap().len() < 100);
        assert!(root.join("journal-ancien.txt").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
