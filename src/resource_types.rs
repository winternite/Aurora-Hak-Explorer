// BioWare Aurora resource type identifiers used by NWN and NWN:EE.
// Unknown identifiers are preserved and displayed as hexadecimal values.
const TYPES: &[(u16, &str)] = &[
    (0x0000, "res"),
    (0x0001, "bmp"),
    (0x0002, "mve"),
    (0x0003, "tga"),
    (0x0004, "wav"),
    (0x0005, "wfx"),
    (0x0006, "plt"),
    (0x0007, "ini"),
    (0x0008, "bmu"),
    (0x0009, "mpg"),
    (0x000a, "txt"),
    (0x07d0, "plh"),
    (0x07d1, "tex"),
    (0x07d2, "mdl"),
    (0x07d3, "thg"),
    (0x07d5, "fnt"),
    (0x07d7, "lua"),
    (0x07d8, "slt"),
    (0x07d9, "nss"),
    (0x07da, "ncs"),
    (0x07db, "mod"),
    (0x07dc, "are"),
    (0x07dd, "set"),
    (0x07de, "ifo"),
    (0x07df, "bic"),
    (0x07e0, "wok"),
    (0x07e1, "2da"),
    (0x07e2, "tlk"),
    (0x07e6, "txi"),
    (0x07e7, "git"),
    (0x07e8, "bti"),
    (0x07e9, "uti"),
    (0x07ea, "btc"),
    (0x07eb, "utc"),
    (0x07ed, "dlg"),
    (0x07ee, "itp"),
    (0x07ef, "btt"),
    (0x07f0, "utt"),
    (0x07f1, "dds"),
    (0x07f2, "bts"),
    (0x07f3, "uts"),
    (0x07f4, "ltr"),
    (0x07f5, "gff"),
    (0x07f6, "fac"),
    (0x07f7, "bte"),
    (0x07f8, "ute"),
    (0x07f9, "btd"),
    (0x07fa, "utd"),
    (0x07fb, "btp"),
    (0x07fc, "utp"),
    (0x07fd, "dft"),
    (0x07fe, "gic"),
    (0x07ff, "gui"),
    (0x0800, "css"),
    (0x0801, "ccs"),
    (0x0802, "btm"),
    (0x0803, "utm"),
    (0x0804, "dwk"),
    (0x0805, "pwk"),
    (0x0806, "btg"),
    (0x0807, "utg"),
    (0x0808, "jrl"),
    (0x0809, "sav"),
    (0x080a, "utw"),
    (0x080b, "4pc"),
    (0x080c, "ssf"),
    (0x080d, "hak"),
    (0x080e, "nwm"),
    (0x080f, "bik"),
    (0x0810, "ndb"),
    (0x0811, "ptm"),
    (0x0812, "ptt"),
    (0x0813, "bak"),
    (0x0814, "dat"),
    (0x0815, "shd"),
    (0x0816, "xbc"),
    (0x0817, "wbm"),
    (0x0818, "mtr"),
    (0x0819, "ktx"),
    (0x081a, "ttf"),
    (0x081b, "sql"),
    (0x081c, "tml"),
    (0x081d, "sq3"),
    (0x081e, "lod"),
    (0x081f, "gif"),
    (0x0820, "png"),
    (0x0821, "jpg"),
    (0x0822, "caf"),
    (0x0823, "jui"),
    (0x0bb8, "osc"),
    (0x0bb9, "usc"),
    (0x0bba, "trn"),
    (0x0bbb, "utr"),
    (0x0bbc, "uen"),
    (0x0bbd, "ult"),
    (0x0bbe, "sef"),
    (0x0bbf, "pfx"),
    (0x0bc0, "cam"),
    (0x0bc1, "lfx"),
    (0x0bc2, "bfx"),
    (0x0bc3, "upe"),
    (0x0bc4, "ros"),
    (0x0bc5, "rst"),
    (0x0bc6, "ifx"),
    (0x0bc7, "pfb"),
    (0x0bc8, "zip"),
    (0x0bc9, "wmp"),
    (0x0bca, "bbx"),
    (0x0bcb, "tfx"),
    (0x0bcc, "wlk"),
    (0x0bcd, "xml"),
    (0x0bce, "scc"),
    (0x0bd9, "ptx"),
    (0x0bda, "ltx"),
    (0x0bdb, "trx"),
    (0x0fa0, "mdb"),
    (0x0fa1, "mda"),
    (0x0fa2, "spt"),
    (0x0fa3, "gr2"),
    (0x0fa4, "fxa"),
    (0x0fa5, "fxe"),
    (0x0fa7, "jpg"),
    (0x0fa8, "pwc"),
    (0x270c, "ids"),
    (0x270d, "erf"),
    (0x270e, "bif"),
    (0x270f, "key"),
];

pub fn extension_for(type_id: u16) -> String {
    TYPES
        .iter()
        .find(|(id, _)| *id == type_id)
        .map(|(_, ext)| (*ext).to_owned())
        .unwrap_or_else(|| format!("type_{type_id:04x}"))
}

pub fn type_for(extension: &str) -> Option<u16> {
    let ext = extension.trim_start_matches('.').to_ascii_lowercase();
    if let Some(hex) = ext.strip_prefix("type_") {
        return u16::from_str_radix(hex, 16).ok();
    }
    TYPES
        .iter()
        .find(|(_, known)| *known == ext)
        .map(|(id, _)| *id)
}

/// Whether an extension is an official Neverwinter Nights / Enhanced Edition
/// resource type. Later table entries are kept for opening legacy or NWN2
/// archives, but are deliberately not accepted for new NWN:EE HAK contents.
pub fn is_nwn_ee_extension(extension: &str) -> bool {
    let ext = extension.trim_start_matches('.').to_ascii_lowercase();
    TYPES
        .iter()
        .any(|(id, known)| is_nwn_ee_type(*id) && *known == ext)
}

pub fn is_nwn_ee_type(type_id: u16) -> bool {
    TYPES
        .iter()
        .any(|(known_id, _)| *known_id == type_id && type_id <= 0x0823)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enhanced_edition_types_are_named() {
        assert_eq!(extension_for(0x0008), "bmu");
        assert_eq!(type_for("bmu"), Some(0x0008));
        assert_eq!(type_for("mp3"), None);
        assert_eq!(extension_for(0x0818), "mtr");
        assert_eq!(extension_for(0x0819), "ktx");
        assert_eq!(extension_for(0x0820), "png");
        assert_eq!(extension_for(0x0823), "jui");
        assert_eq!(type_for("png"), Some(0x0820));
        assert!(is_nwn_ee_extension("png"));
        assert!(is_nwn_ee_extension("bmu"));
        assert!(!is_nwn_ee_extension("zip"));
        assert!(!is_nwn_ee_extension("mp3"));
        assert!(is_nwn_ee_type(0x0820));
        assert!(!is_nwn_ee_type(0x0bc8));
    }
}
