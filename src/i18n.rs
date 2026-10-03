//! Tiny runtime translation layer: English, Portuguese (Brazil), Spanish and Greek.
//!
//! * `tr("English text")` returns the text in the current language.
//! * `trf("… {n} …", &[("n", value)])` does the same and fills in `{placeholders}`.
//! * `bind(closure)` runs a closure now and again whenever the language changes,
//!   which is how static widget texts follow a live language switch.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Pt,
    Es,
    El,
}

impl Lang {
    pub const ALL: [Lang; 4] = [Lang::En, Lang::Pt, Lang::Es, Lang::El];

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Pt => "pt",
            Lang::Es => "es",
            Lang::El => "el",
        }
    }

    /// The language's name written in that language.
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Pt => "Português",
            Lang::Es => "Español",
            Lang::El => "Ελληνικά",
        }
    }

    pub fn flag_icon(self) -> &'static str {
        match self {
            Lang::En => "vs-flag-en",
            Lang::Pt => "vs-flag-pt",
            Lang::Es => "vs-flag-es",
            Lang::El => "vs-flag-el",
        }
    }

    fn from_code(code: &str) -> Option<Lang> {
        let c = code.trim().to_lowercase();
        Lang::ALL.into_iter().find(|l| c.starts_with(l.code()))
    }
}

thread_local! {
    static LANG: Cell<Lang> = const { Cell::new(Lang::En) };
    static BINDINGS: RefCell<Vec<Box<dyn Fn()>>> = RefCell::new(Vec::new());
}

pub fn lang() -> Lang {
    LANG.with(|l| l.get())
}

#[cfg(not(windows))]
fn config_file() -> PathBuf {
    gtk::glib::user_config_dir().join("chop-chop").join("language")
}

/// `%APPDATA%\chop-chop\language` on Windows.
#[cfg(windows)]
fn config_file() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("chop-chop").join("language")
}

/// Saved choice first, then the desktop's locale, then English.
pub fn init() {
    init_with_locale(None);
}

/// Like `init`, with the system UI locale (e.g. "pt-BR") as an extra hint when no
/// language was saved and the locale environment variables are unset (Windows).
pub fn init_with_locale(system_locale: Option<String>) {
    let saved = std::fs::read_to_string(config_file()).ok().and_then(|s| Lang::from_code(&s));
    let from_env = || {
        ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
            .and_then(|v| Lang::from_code(&v))
    };
    let from_system = || system_locale.as_deref().and_then(Lang::from_code);
    LANG.with(|l| l.set(saved.or_else(from_env).or_else(from_system).unwrap_or(Lang::En)));
}

/// Switch language, remember it, and re-run every bound text.
pub fn set_lang(lang: Lang) {
    LANG.with(|l| l.set(lang));
    let f = config_file();
    if let Some(dir) = f.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(f, lang.code());
    BINDINGS.with(|b| {
        for f in b.borrow().iter() {
            f();
        }
    });
}

pub fn bind(f: impl Fn() + 'static) {
    f();
    BINDINGS.with(|b| b.borrow_mut().push(Box::new(f)));
}

/// Uppercase for card titles; Greek capitals drop the accent (tonos) by convention.
pub fn upper(s: &str) -> String {
    s.to_uppercase()
        .chars()
        .map(|c| match c {
            'Ά' => 'Α',
            'Έ' => 'Ε',
            'Ή' => 'Η',
            'Ί' => 'Ι',
            'Ό' => 'Ο',
            'Ύ' => 'Υ',
            'Ώ' => 'Ω',
            other => other,
        })
        .collect()
}

pub fn tr(en: &str) -> String {
    let idx = match lang() {
        Lang::En => return en.to_string(),
        Lang::Pt => 0,
        Lang::Es => 1,
        Lang::El => 2,
    };
    TABLE
        .iter()
        .find(|row| row.0 == en)
        .map(|row| [row.1, row.2, row.3][idx].to_string())
        .unwrap_or_else(|| en.to_string())
}

pub fn trf(en: &str, args: &[(&str, String)]) -> String {
    let mut s = tr(en);
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// (English, Português, Español, Ελληνικά)
const TABLE: &[(&str, &str, &str, &str)] = &[
    ("Open", "Abrir", "Abrir", "Άνοιγμα"),
    ("Open a video (Ctrl+O)", "Abrir um vídeo (Ctrl+O)", "Abrir un vídeo (Ctrl+O)", "Άνοιγμα βίντεο (Ctrl+O)"),
    ("Show output folder", "Mostrar pasta de saída", "Mostrar carpeta de salida", "Εμφάνιση φακέλου εξόδου"),
    ("Language", "Idioma", "Idioma", "Γλώσσα"),
    ("Drop a video here…", "Solte um vídeo aqui…", "Suelta un vídeo aquí…", "Αφήστε ένα βίντεο εδώ…"),
    ("Preview", "Pré-visualização", "Vista previa", "Προεπισκόπηση"),
    ("Custom selection", "Seleção personalizada", "Selección personalizada", "Προσαρμοσμένη επιλογή"),
    ("Batch split", "Divisão em lote", "División por lotes", "Μαζικός διαχωρισμός"),
    ("NO VIDEO", "SEM VÍDEO", "SIN VÍDEO", "ΧΩΡΙΣ ΒΙΝΤΕΟ"),
    ("START", "INÍCIO", "INICIO", "ΑΡΧΗ"),
    ("END", "FIM", "FIN", "ΤΕΛΟΣ"),
    ("Start", "Início", "Inicio", "Αρχή"),
    ("End", "Fim", "Fin", "Τέλος"),
    (
        "Which frame the preview shows",
        "Qual quadro a pré-visualização mostra",
        "Qué fotograma muestra la vista previa",
        "Ποιο καρέ δείχνει η προεπισκόπηση",
    ),
    (
        "Drag the handles to choose the start and end of the clip",
        "Arraste as alças para escolher o início e o fim do clipe",
        "Arrastra los controles para elegir el inicio y el final del clip",
        "Σύρετε τις λαβές για να επιλέξετε την αρχή και το τέλος του αποσπάσματος",
    ),
    ("Start time", "Tempo inicial", "Tiempo inicial", "Χρόνος έναρξης"),
    ("End time", "Tempo final", "Tiempo final", "Χρόνος λήξης"),
    ("Split the whole video", "Dividir o vídeo inteiro", "Dividir el vídeo completo", "Διαχωρισμός ολόκληρου του βίντεο"),
    ("Every…", "A cada…", "Cada…", "Κάθε…"),
    ("Equal parts", "Partes iguais", "Partes iguales", "Ίσα μέρη"),
    ("min", "min", "min", "λεπ."),
    ("sec", "s", "s", "δευτ."),
    ("equal parts", "partes iguais", "partes iguales", "ίσα μέρη"),
    ("Export", "Exportar", "Exportar", "Εξαγωγή"),
    ("Export audio…", "Exportar áudio…", "Exportar audio…", "Εξαγωγή ήχου…"),
    ("Parallel workers", "Processos paralelos", "Procesos en paralelo", "Παράλληλες εργασίες"),
    ("{n} cores detected", "{n} núcleos detectados", "{n} núcleos detectados", "{n} πυρήνες εντοπίστηκαν"),
    ("Output folder", "Pasta de saída", "Carpeta de salida", "Φάκελος εξόδου"),
    ("Change…", "Alterar…", "Cambiar…", "Αλλαγή…"),
    ("File name", "Nome do arquivo", "Nombre del archivo", "Όνομα αρχείου"),
    (
        "Open the output folder when finished",
        "Abrir a pasta de saída ao terminar",
        "Abrir la carpeta de salida al terminar",
        "Άνοιγμα του φακέλου εξόδου μετά την ολοκλήρωση",
    ),
    ("Cut clip", "Cortar clipe", "Cortar clip", "Αποκοπή αποσπάσματος"),
    ("Cancel", "Cancelar", "Cancelar", "Ακύρωση"),
    ("Split video", "Dividir vídeo", "Dividir vídeo", "Διαχωρισμός βίντεο"),
    ("Export 1 part", "Exportar 1 parte", "Exportar 1 parte", "Εξαγωγή 1 μέρους"),
    ("Split into {n} parts", "Dividir em {n} partes", "Dividir en {n} partes", "Διαχωρισμός σε {n} μέρη"),
    ("Progress", "Progresso", "Progreso", "Πρόοδος"),
    (
        "Encoding jobs will appear here.",
        "As tarefas de codificação aparecerão aqui.",
        "Las tareas de codificación aparecerán aquí.",
        "Οι εργασίες κωδικοποίησης θα εμφανιστούν εδώ.",
    ),
    ("Reading video information…", "Lendo informações do vídeo…", "Leyendo información del vídeo…", "Ανάγνωση πληροφοριών βίντεο…"),
    ("Cancelling…", "Cancelando…", "Cancelando…", "Ακύρωση…"),
    (
        "Encoding {len} — split into {n} chunk(s) so every core helps…",
        "Codificando {len} — dividido em {n} trecho(s) para usar todos os núcleos…",
        "Codificando {len} — dividido en {n} fragmento(s) para usar todos los núcleos…",
        "Κωδικοποίηση {len} — σε {n} τμήμα(τα) ώστε να βοηθούν όλοι οι πυρήνες…",
    ),
    (
        "Splitting into {n} parts, {w} at a time…",
        "Dividindo em {n} partes, {w} por vez…",
        "Dividiendo en {n} partes, {w} a la vez…",
        "Διαχωρισμός σε {n} μέρη, {w} τη φορά…",
    ),
    ("Exporting {fmt} audio…", "Exportando áudio {fmt}…", "Exportando audio {fmt}…", "Εξαγωγή ήχου {fmt}…"),
    (
        "Exporting {n} {fmt} files, {w} at a time…",
        "Exportando {n} arquivos {fmt}, {w} por vez…",
        "Exportando {n} archivos {fmt}, {w} a la vez…",
        "Εξαγωγή {n} αρχείων {fmt}, {w} τη φορά…",
    ),
    (
        "{t} elapsed · about {left} left",
        "{t} decorrido · cerca de {left} restante",
        "{t} transcurrido · quedan unos {left}",
        "Πέρασαν {t} · απομένουν περίπου {left}",
    ),
    (
        "Saved {what} · {size} · done in {secs} s",
        "Salvo: {what} · {size} · concluído em {secs} s",
        "Guardado: {what} · {size} · terminado en {secs} s",
        "Αποθηκεύτηκε: {what} · {size} · ολοκληρώθηκε σε {secs} δευτ.",
    ),
    ("{n} files", "{n} arquivos", "{n} archivos", "{n} αρχεία"),
    (
        "Cancelled — no files were kept.",
        "Cancelado — nenhum arquivo foi mantido.",
        "Cancelado — no se conservó ningún archivo.",
        "Ακυρώθηκε — δεν κρατήθηκε κανένα αρχείο.",
    ),
    ("Failed: {e}", "Falhou: {e}", "Error: {e}", "Αποτυχία: {e}"),
    ("Queued", "Na fila", "En cola", "Σε αναμονή"),
    ("Working", "Processando", "Procesando", "Σε εξέλιξη"),
    ("Done", "Concluído", "Listo", "Έτοιμο"),
    ("Stopped", "Parado", "Detenido", "Διακόπηκε"),
    ("Failed", "Falhou", "Error", "Απέτυχε"),
    ("Parallel chunk {i}/{n}", "Trecho paralelo {i}/{n}", "Fragmento paralelo {i}/{n}", "Παράλληλο τμήμα {i}/{n}"),
    ("Audio track", "Faixa de áudio", "Pista de audio", "Κομμάτι ήχου"),
    ("Join", "Juntar", "Unir", "Ένωση"),
    ("Lossless concat + mux", "Junção sem perdas", "Unión sin pérdidas", "Ένωση χωρίς απώλειες"),
    ("Part {i}", "Parte {i}", "Parte {i}", "Μέρος {i}"),
    ("{fmt} audio", "Áudio {fmt}", "Audio {fmt}", "Ήχος {fmt}"),
    ("Part {i} · {fmt}", "Parte {i} · {fmt}", "Parte {i} · {fmt}", "Μέρος {i} · {fmt}"),
    ("FFmpeg was not found", "O FFmpeg não foi encontrado", "No se encontró FFmpeg", "Δεν βρέθηκε το FFmpeg"),
    (
        "Chop Chop Splitter uses FFmpeg to cut videos. Install it with:\n\nsudo apt install ffmpeg",
        "O Chop Chop Splitter usa o FFmpeg para cortar vídeos. Instale-o com:\n\nsudo apt install ffmpeg",
        "Chop Chop Splitter usa FFmpeg para cortar vídeos. Instálalo con:\n\nsudo apt install ffmpeg",
        "Το Chop Chop Splitter χρησιμοποιεί το FFmpeg για την αποκοπή βίντεο. Εγκαταστήστε το με:\n\nsudo apt install ffmpeg",
    ),
    ("Can't open this video", "Não foi possível abrir este vídeo", "No se puede abrir este vídeo", "Δεν είναι δυνατό το άνοιγμα του βίντεο"),
    ("The clip is too short", "O clipe é muito curto", "El clip es demasiado corto", "Το απόσπασμα είναι πολύ σύντομο"),
    (
        "Choose an end time after the start time.",
        "Escolha um tempo final depois do tempo inicial.",
        "Elige un tiempo final posterior al inicial.",
        "Επιλέξτε χρόνο λήξης μετά τον χρόνο έναρξης.",
    ),
    ("Nothing to split", "Nada para dividir", "Nada que dividir", "Τίποτα για διαχωρισμό"),
    (
        "Choose a part length or a number of parts.",
        "Escolha a duração das partes ou o número de partes.",
        "Elige la duración de las partes o el número de partes.",
        "Επιλέξτε διάρκεια μέρους ή αριθμό μερών.",
    ),
    ("No output folder", "Nenhuma pasta de saída", "Sin carpeta de salida", "Δεν υπάρχει φάκελος εξόδου"),
    (
        "Please choose where the files should be saved.",
        "Escolha onde os arquivos devem ser salvos.",
        "Elige dónde guardar los archivos.",
        "Επιλέξτε πού θα αποθηκευτούν τα αρχεία.",
    ),
    (
        "Output folder is not writable",
        "Não é possível gravar na pasta de saída",
        "No se puede escribir en la carpeta de salida",
        "Δεν είναι δυνατή η εγγραφή στον φάκελο εξόδου",
    ),
    ("Choose another file name", "Escolha outro nome de arquivo", "Elige otro nombre de archivo", "Επιλέξτε άλλο όνομα αρχείου"),
    (
        "The export would overwrite the original video.",
        "A exportação substituiria o vídeo original.",
        "La exportación sobrescribiría el vídeo original.",
        "Η εξαγωγή θα αντικαθιστούσε το αρχικό βίντεο.",
    ),
    ("Replace existing file?", "Substituir o arquivo existente?", "¿Reemplazar el archivo existente?", "Αντικατάσταση υπάρχοντος αρχείου;"),
    ("Replace existing files?", "Substituir os arquivos existentes?", "¿Reemplazar los archivos existentes?", "Αντικατάσταση υπαρχόντων αρχείων;"),
    ("Already in this folder:", "Já existem nesta pasta:", "Ya están en esta carpeta:", "Υπάρχουν ήδη σε αυτόν τον φάκελο:"),
    ("…and {n} more", "…e mais {n}", "…y {n} más", "…και {n} ακόμη"),
    ("Open a video", "Abrir um vídeo", "Abrir un vídeo", "Άνοιγμα βίντεο"),
    ("_Open", "_Abrir", "_Abrir", "Ά_νοιγμα"),
    ("_Cancel", "_Cancelar", "_Cancelar", "Ά_κυρο"),
    ("Videos", "Vídeos", "Vídeos", "Βίντεο"),
    ("All files", "Todos os arquivos", "Todos los archivos", "Όλα τα αρχεία"),
    ("Choose the output folder", "Escolha a pasta de saída", "Elige la carpeta de salida", "Επιλογή φακέλου εξόδου"),
    ("_Select", "_Selecionar", "_Seleccionar", "_Επιλογή"),
    ("Export audio", "Exportar áudio", "Exportar audio", "Εξαγωγή ήχου"),
    (
        "Export audio — one file per part",
        "Exportar áudio — um arquivo por parte",
        "Exportar audio — un archivo por parte",
        "Εξαγωγή ήχου — ένα αρχείο ανά μέρος",
    ),
    ("_Export", "_Exportar", "_Exportar", "_Εξαγωγή"),
    (
        "Save the selected range as WAV, MP3, OGG, FLAC, M4A or OPUS",
        "Salvar o trecho selecionado como WAV, MP3, OGG, FLAC, M4A ou OPUS",
        "Guardar el rango seleccionado como WAV, MP3, OGG, FLAC, M4A u OPUS",
        "Αποθήκευση του επιλεγμένου τμήματος ως WAV, MP3, OGG, FLAC, M4A ή OPUS",
    ),
    (
        "Save one audio file per part as WAV, MP3, OGG, FLAC, M4A or OPUS",
        "Salvar um arquivo de áudio por parte como WAV, MP3, OGG, FLAC, M4A ou OPUS",
        "Guardar un archivo de audio por parte como WAV, MP3, OGG, FLAC, M4A u OPUS",
        "Αποθήκευση ενός αρχείου ήχου ανά μέρος ως WAV, MP3, OGG, FLAC, M4A ή OPUS",
    ),
    (
        "This video has no audio track",
        "Este vídeo não tem faixa de áudio",
        "Este vídeo no tiene pista de audio",
        "Αυτό το βίντεο δεν έχει κομμάτι ήχου",
    ),
    ("PCM 16-bit, uncompressed", "PCM 16 bits, sem compressão", "PCM de 16 bits, sin comprimir", "PCM 16-bit, χωρίς συμπίεση"),
    ("Lossless", "Sem perdas", "Sin pérdidas", "Χωρίς απώλειες"),
    (
        "Chop Chop Splitter uses FFmpeg to cut videos. Reinstall Chop Chop Splitter, or put ffmpeg.exe and ffprobe.exe next to chop-chop.exe or on your PATH.",
        "O Chop Chop Splitter usa o FFmpeg para cortar vídeos. Reinstale o Chop Chop Splitter ou coloque ffmpeg.exe e ffprobe.exe ao lado de chop-chop.exe ou no PATH.",
        "Chop Chop Splitter usa FFmpeg para cortar vídeos. Reinstala Chop Chop Splitter o coloca ffmpeg.exe y ffprobe.exe junto a chop-chop.exe o en el PATH.",
        "Το Chop Chop Splitter χρησιμοποιεί το FFmpeg για την αποκοπή βίντεο. Εγκαταστήστε ξανά το Chop Chop Splitter ή τοποθετήστε τα ffmpeg.exe και ffprobe.exe δίπλα στο chop-chop.exe ή στο PATH.",
    ),
    // Windows build: group boxes and job list columns.
    ("Video", "Vídeo", "Vídeo", "Βίντεο"),
    ("Job", "Tarefa", "Tarea", "Εργασία"),
    ("Range", "Intervalo", "Intervalo", "Διάστημα"),
    ("Status", "Status", "Estado", "Κατάσταση"),
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn placeholders_survive_translation() {
        for row in TABLE {
            for t in [row.1, row.2, row.3] {
                for ph in ["{n}", "{w}", "{fmt}", "{len}", "{t}", "{left}", "{what}", "{size}", "{secs}", "{e}", "{i}"] {
                    assert_eq!(row.0.contains(ph), t.contains(ph), "placeholder {ph} mismatch in {:?}", row.0);
                }
            }
        }
    }
}
