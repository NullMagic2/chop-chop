//! Native Windows dialogs: Common Item Dialog (open / save / pick folder) and TaskDialog,
//! standing in for GTK's FileChooserNative and MessageDialog.

use std::path::{Path, PathBuf};

use windows::core::{Interface, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Controls::{
    TaskDialog, TASKDIALOG_COMMON_BUTTON_FLAGS, TDCBF_CLOSE_BUTTON, TDCBF_NO_BUTTON, TDCBF_YES_BUTTON, TD_ERROR_ICON,
    TD_WARNING_ICON,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FileOpenDialog, FileSaveDialog, IFileDialog, IFileOpenDialog, IFileSaveDialog, IShellItem,
    SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::IDYES;

pub struct Filter {
    pub name: String,
    /// "*.mp4;*.mkv"
    pub spec: String,
}

fn shell_item(path: &Path) -> Option<IShellItem> {
    unsafe { SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None).ok() }
}

fn item_path(item: &IShellItem) -> Option<PathBuf> {
    unsafe {
        let p: PWSTR = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s.map(PathBuf::from)
    }
}

fn set_filters(dlg: &IFileDialog, filters: &[Filter]) {
    // The HSTRINGs must outlive SetFileTypes (the dialog copies them).
    let names: Vec<HSTRING> = filters.iter().map(|f| HSTRING::from(f.name.as_str())).collect();
    let specs: Vec<HSTRING> = filters.iter().map(|f| HSTRING::from(f.spec.as_str())).collect();
    let rg: Vec<COMDLG_FILTERSPEC> = names
        .iter()
        .zip(&specs)
        .map(|(n, s)| COMDLG_FILTERSPEC { pszName: PCWSTR(n.as_ptr()), pszSpec: PCWSTR(s.as_ptr()) })
        .collect();
    unsafe {
        let _ = dlg.SetFileTypes(&rg);
    }
}

pub fn open_file(owner: HWND, title: &str, filters: &[Filter]) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let fd: IFileDialog = dlg.cast().ok()?;
        let _ = fd.SetTitle(&HSTRING::from(title));
        set_filters(&fd, filters);
        let opts = fd.GetOptions().ok()?;
        let _ = fd.SetOptions(opts | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST);
        fd.Show(Some(owner)).ok()?;
        item_path(&fd.GetResult().ok()?)
    }
}

pub fn pick_folder(owner: HWND, title: &str, current: Option<&Path>) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let fd: IFileDialog = dlg.cast().ok()?;
        let _ = fd.SetTitle(&HSTRING::from(title));
        let opts = fd.GetOptions().ok()?;
        let _ = fd.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST);
        if let Some(item) = current.and_then(shell_item) {
            let _ = fd.SetFolder(&item);
        }
        fd.Show(Some(owner)).ok()?;
        item_path(&fd.GetResult().ok()?)
    }
}

/// Save dialog. Returns the chosen path and the 0-based index of the selected file type.
pub fn save_file(
    owner: HWND,
    title: &str,
    folder: Option<&Path>,
    name: &str,
    filters: &[Filter],
    selected: usize,
    default_ext: &str,
    overwrite_prompt: bool,
) -> Option<(PathBuf, usize)> {
    unsafe {
        let dlg: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let fd: IFileDialog = dlg.cast().ok()?;
        let _ = fd.SetTitle(&HSTRING::from(title));
        set_filters(&fd, filters);
        let _ = fd.SetFileTypeIndex(selected as u32 + 1);
        // With a default extension, the dialog swaps the extension when another type is picked.
        let _ = fd.SetDefaultExtension(&HSTRING::from(default_ext));
        let opts = fd.GetOptions().ok()?;
        let mut o = opts | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST;
        if overwrite_prompt {
            o |= FOS_OVERWRITEPROMPT;
        } else {
            o &= !FOS_OVERWRITEPROMPT;
        }
        let _ = fd.SetOptions(o);
        if let Some(item) = folder.and_then(shell_item) {
            let _ = fd.SetFolder(&item);
        }
        let _ = fd.SetFileName(&HSTRING::from(name));
        fd.Show(Some(owner)).ok()?;
        let idx = fd.GetFileTypeIndex().map(|i| i.saturating_sub(1) as usize).unwrap_or(selected);
        Some((item_path(&fd.GetResult().ok()?)?, idx))
    }
}

fn task_dialog(owner: HWND, title: &str, body: &str, buttons: TASKDIALOG_COMMON_BUTTON_FLAGS, icon: PCWSTR) -> i32 {
    let mut pressed = 0;
    unsafe {
        let res = TaskDialog(
            Some(owner),
            None,
            &HSTRING::from("Chop Chop Splitter"),
            &HSTRING::from(title),
            &HSTRING::from(body),
            buttons,
            icon,
            Some(&mut pressed),
        );
        if res.is_err() {
            // TaskDialog needs Common Controls 6; fall back to a plain message box.
            use windows::Win32::UI::WindowsAndMessaging::*;
            let text = HSTRING::from(format!("{title}\n\n{body}"));
            let style = if buttons.0 & TDCBF_YES_BUTTON.0 != 0 { MB_YESNO | MB_ICONQUESTION } else { MB_OK | MB_ICONERROR };
            return MessageBoxW(Some(owner), &text, &HSTRING::from("Chop Chop Splitter"), style).0;
        }
    }
    pressed
}

pub fn error(owner: HWND, title: &str, body: &str) {
    task_dialog(owner, title, body, TDCBF_CLOSE_BUTTON, TD_ERROR_ICON);
}

pub fn confirm(owner: HWND, title: &str, body: &str) -> bool {
    task_dialog(owner, title, body, TDCBF_YES_BUTTON | TDCBF_NO_BUTTON, TD_WARNING_ICON) == IDYES.0
}
