use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{ffi::c_void, process::Command, ptr};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Source {
    id: String,
    bundle: Option<String>,
    mode: Option<String>,
    selectable: bool,
    enabled: bool,
    ascii: bool,
}
impl Source {
    fn kanade(&self) -> bool {
        self.bundle.as_deref().is_some_and(|id| {
            matches!(
                id,
                "dev.kanade.inputmethod" | "dev.kanade.inputmethod.Kanade"
            )
        })
    }
    fn same_selection(&self, other: &Self) -> bool {
        self.id == other.id && self.mode == other.mode
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Lease {
    original: Source,
    temporary: Source,
}
#[derive(Deserialize, Serialize)]
enum Request {
    Prepare,
    SelectTemporary(Lease),
    Current,
    Restore(Lease),
}
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
enum Restoration {
    Restored(Source),
    UserChanged,
}

// Short-lived helpers avoid TIS's process-local source-list cache across registration.
fn invoke<T: serde::de::DeserializeOwned>(request: Request) -> Result<T> {
    let result = Command::new(std::env::current_exe()?)
        .args([
            "install-macos",
            "--input-source-helper",
            &serde_json::to_string(&request)?,
        ])
        .output()?;
    ensure!(
        result.status.success(),
        "input-source operation failed: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    serde_json::from_slice(&result.stdout).context("invalid input-source helper response")
}
trait Backend {
    fn prepare(&mut self) -> Result<Option<Lease>>;
    fn select_temporary(&mut self, lease: &Lease) -> Result<()>;
    fn current(&mut self) -> Result<Source>;
    fn confirm(&mut self, lease: &Lease) -> Result<Source> {
        let selected = self.current()?;
        ensure!(
            selected.same_selection(&lease.temporary),
            "ASCII input was not confirmed; update was not started"
        );
        Ok(selected)
    }
    fn restore(&mut self, lease: &Lease) -> Result<Restoration>;
}
struct Native;
impl Backend for Native {
    fn prepare(&mut self) -> Result<Option<Lease>> {
        invoke(Request::Prepare)
    }
    fn select_temporary(&mut self, lease: &Lease) -> Result<()> {
        let _: bool = invoke(Request::SelectTemporary(lease.clone()))?;
        Ok(())
    }
    fn current(&mut self) -> Result<Source> {
        invoke(Request::Current)
    }
    fn confirm(&mut self, lease: &Lease) -> Result<Source> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let selected = self.current()?;
            if selected.same_selection(&lease.temporary) {
                return Ok(selected);
            }
            if !selected.same_selection(&lease.original) || std::time::Instant::now() >= deadline {
                bail_confirmation(&lease.temporary, &selected)?;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    fn restore(&mut self, lease: &Lease) -> Result<Restoration> {
        let result = invoke(Request::Restore(lease.clone()))?;
        if let Restoration::Restored(expected) = &result {
            let current = self.current()?;
            ensure!(
                current.same_selection(expected)
                    || (current.kanade()
                        && current.bundle == expected.bundle
                        && current.id == expected.bundle.as_deref().unwrap_or("")
                        && current.mode.is_none()),
                "Kanade was installed but its input source could not be restored"
            );
        }
        Ok(result)
    }
}
struct Guard<'a, B: Backend> {
    backend: &'a mut B,
    lease: Option<Lease>,
}
impl<B: Backend> Guard<'_, B> {
    fn restore(&mut self) -> Result<()> {
        if let Some(lease) = self.lease.take() {
            match self.backend.restore(&lease)? {
                Restoration::Restored(_) => println!("restored the Kanade input source"),
                Restoration::UserChanged => {
                    println!("kept the input source selected during installation")
                }
            }
        }
        Ok(())
    }
}
impl<B: Backend> Drop for Guard<'_, B> {
    fn drop(&mut self) {
        if self.restore().is_err() {
            eprintln!("input source could not be restored; select Kanade from the input menu");
        }
    }
}
fn transaction<B: Backend, T>(backend: &mut B, update: impl FnOnce() -> Result<T>) -> Result<T> {
    let lease = backend.prepare()?;
    let mut guard = Guard { backend, lease };
    let result = (|| {
        if let Some(lease) = &guard.lease {
            guard.backend.select_temporary(lease)?;
            guard.backend.confirm(lease)?;
            println!("selected an ASCII keyboard layout for installation");
        }
        update()
    })();
    match result {
        Ok(value) => {
            if let Some(lease) = guard.lease.take() {
                if guard.backend.current().is_ok_and(|current| current.same_selection(&lease.temporary)) {
                    println!("ASCII input remains selected; press Kana or select Kanade from the input menu to resume");
                } else {
                    println!("kept the input source selected during installation");
                }
            }
            Ok(value)
        }
        Err(error) => match guard.restore() {
            Ok(()) => Err(error),
            Err(restore) => Err(error.context(format!("input-source restoration also failed: {restore:#}; select Kanade from the input menu"))),
        },
    }
}
pub(super) fn during_update<T>(update: impl FnOnce() -> Result<T>) -> Result<T> {
    transaction(&mut Native, update)
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardInputSource() -> *const c_void;
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> *const c_void;
    fn TISCreateInputSourceList(properties: *const c_void, all: u8) -> *const c_void;
    fn TISGetInputSourceProperty(source: *const c_void, key: *const c_void) -> *const c_void;
    fn TISSelectInputSource(source: *const c_void) -> i32;
    fn TISEnableInputSource(source: *const c_void) -> i32;
    static kTISPropertyInputSourceID: *const c_void;
    static kTISPropertyBundleID: *const c_void;
    static kTISPropertyInputModeID: *const c_void;
    static kTISPropertyInputSourceIsSelectCapable: *const c_void;
    static kTISPropertyInputSourceIsEnabled: *const c_void;
    static kTISPropertyInputSourceIsASCIICapable: *const c_void;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const c_void);
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFBooleanGetValue(value: *const c_void) -> u8;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFStringGetCString(value: *const c_void, buffer: *mut u8, size: isize, encoding: u32) -> u8;
}
struct Owned(*const c_void);
impl Owned {
    fn new(value: *const c_void) -> Result<Self> {
        ensure!(!value.is_null(), "input source unavailable");
        Ok(Self(value))
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}
fn describe(source: *const c_void) -> Result<Source> {
    let string = |key| -> Option<String> {
        let value = unsafe { TISGetInputSourceProperty(source, key) };
        let mut bytes = [0u8; 1024];
        if value.is_null()
            || unsafe {
                CFStringGetCString(value, bytes.as_mut_ptr(), bytes.len() as isize, 0x08000100)
            } == 0
        {
            return None;
        }
        let end = bytes.iter().position(|b| *b == 0)?;
        String::from_utf8(bytes[..end].to_vec()).ok()
    };
    let boolean = |key| {
        let value = unsafe { TISGetInputSourceProperty(source, key) };
        !value.is_null() && unsafe { CFBooleanGetValue(value) } != 0
    };
    Ok(unsafe {
        Source {
            id: string(kTISPropertyInputSourceID).context("input source has no identifier")?,
            bundle: string(kTISPropertyBundleID),
            mode: string(kTISPropertyInputModeID),
            selectable: boolean(kTISPropertyInputSourceIsSelectCapable),
            enabled: boolean(kTISPropertyInputSourceIsEnabled),
            ascii: boolean(kTISPropertyInputSourceIsASCIICapable),
        }
    })
}
fn current() -> Result<Source> {
    // Initialize the TIS observer, then deliver queued distributed notifications
    // before reading its process-local selection cache.
    let initial = Owned::new(unsafe { TISCopyCurrentKeyboardInputSource() })?;
    objc2_foundation::NSRunLoop::currentRunLoop()
        .runUntilDate(&objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(0.1));
    drop(initial);
    describe(Owned::new(unsafe { TISCopyCurrentKeyboardInputSource() })?.0)
}
fn bail_confirmation(expected: &Source, observed: &Source) -> Result<()> {
    anyhow::bail!(
        "ASCII input was not confirmed; update was not started (expected source {} mode {:?}, observed source {} mode {:?})",
        expected.id,
        expected.mode,
        observed.id,
        observed.mode
    )
}
struct Sources {
    _array: Owned,
    pointers: Vec<*const c_void>,
    metadata: Vec<Source>,
}
fn sources(bundle: &str) -> Result<Sources> {
    let id = objc2_foundation::NSString::from_str(bundle);
    let keys = [unsafe { kTISPropertyBundleID }];
    let values = [(&*id as *const objc2_foundation::NSString).cast::<c_void>()];
    let filter = Owned::new(unsafe {
        CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    })?;
    // Enabled-only enumeration can omit a mode even when its enabled property is true.
    let array = Owned::new(unsafe { TISCreateInputSourceList(filter.0, 1) })?;
    let pointers: Vec<_> = (0..unsafe { CFArrayGetCount(array.0) })
        .map(|index| unsafe { CFArrayGetValueAtIndex(array.0, index) })
        .collect();
    let metadata = pointers
        .iter()
        .map(|source| describe(*source))
        .collect::<Result<Vec<_>>>()?;
    Ok(Sources {
        _array: array,
        pointers,
        metadata,
    })
}
fn restore_index(sources: &[Source], original: &Source) -> Result<usize> {
    let candidates: Vec<_> = sources
        .iter()
        .enumerate()
        .filter(|(_, source)| {
            source.kanade()
                && source.bundle == original.bundle
                && source.enabled
                && source.selectable
        })
        .collect();
    if let Some((index, _)) = candidates
        .iter()
        .find(|(_, source)| source.same_selection(original))
    {
        return Ok(*index);
    }
    let modes: Vec<_> = candidates
        .iter()
        .filter(|(_, source)| original.mode.is_none() || source.mode == original.mode)
        .collect();
    ensure!(
        modes.len() == 1,
        "the original Kanade input mode cannot be uniquely restored"
    );
    Ok(modes[0].0)
}
fn select(source: *const c_void) -> Result<()> {
    let metadata = describe(source)?;
    ensure!(
        metadata.enabled && metadata.selectable,
        "input source is not enabled and selectable"
    );
    ensure!(
        unsafe { TISSelectInputSource(source) } == 0,
        "input-source selection failed"
    );
    settle();
    Ok(())
}
fn settle() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        objc2_foundation::NSRunLoop::currentRunLoop()
            .runUntilDate(&objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(0.1));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
pub(super) fn helper() -> Result<bool> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(2).map(String::as_str) != Some("--input-source-helper") {
        return Ok(false);
    }
    let request: Request =
        serde_json::from_str(args.get(3).context("input-source request missing")?)?;
    let value = match request {
        Request::Current => serde_json::to_value(current()?)?,
        Request::Prepare => {
            let original = current()?;
            let lease = if original.kanade() {
                let layout =
                    Owned::new(unsafe { TISCopyCurrentASCIICapableKeyboardLayoutInputSource() })?;
                let temporary = describe(layout.0)?;
                ensure!(
                    temporary.enabled
                        && temporary.selectable
                        && temporary.ascii
                        && !temporary.kanade(),
                    "no enabled ASCII keyboard layout is available"
                );
                Some(Lease {
                    original,
                    temporary,
                })
            } else {
                None
            };
            serde_json::to_value(lease)?
        }
        Request::SelectTemporary(lease) => {
            ensure!(
                lease.original.kanade() && !lease.temporary.kanade() && lease.temporary.ascii,
                "invalid temporary input-source request"
            );
            ensure!(
                current()?.same_selection(&lease.original),
                "input source changed before selection"
            );
            let layout =
                Owned::new(unsafe { TISCopyCurrentASCIICapableKeyboardLayoutInputSource() })?;
            ensure!(
                describe(layout.0)?.same_selection(&lease.temporary),
                "ASCII keyboard layout changed before selection"
            );
            select(layout.0)
                .context("could not select the ASCII keyboard layout; update was not started")?;
            serde_json::Value::Bool(true)
        }
        Request::Restore(lease) => {
            ensure!(
                lease.original.kanade() && !lease.temporary.kanade() && lease.temporary.ascii,
                "invalid input-source restoration request"
            );
            let result = if !current()?.same_selection(&lease.temporary) {
                Restoration::UserChanged
            } else {
                let sources = sources(
                    lease
                        .original
                        .bundle
                        .as_deref()
                        .context("input-source bundle missing")?,
                )?;
                let index = restore_index(&sources.metadata, &lease.original)?;
                if let Some(parent) = sources.metadata.iter().position(|source| {
                    source.id == lease.original.id && source.enabled && !source.selectable
                }) {
                    ensure!(
                        unsafe { TISEnableInputSource(sources.pointers[parent]) } == 0,
                        "enabled Kanade input method could not be refreshed"
                    );
                }
                select(sources.pointers[index])
                    .context("could not restore the Kanade input mode")?;
                Restoration::Restored(sources.metadata[index].clone())
            };
            serde_json::to_value(result)?
        }
    };
    println!("{}", serde_json::to_string(&value)?);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    fn source(id: &str, kanade: bool) -> Source {
        Source {
            id: id.into(),
            bundle: kanade.then(|| "dev.kanade.inputmethod.Kanade".into()),
            mode: None,
            selectable: true,
            enabled: true,
            ascii: !kanade,
        }
    }
    struct Fake {
        lease: Option<Lease>,
        current: Source,
        restored: bool,
        fail_prepare: bool,
        fail_select: bool,
        fail_restore: bool,
    }
    impl Backend for Fake {
        fn prepare(&mut self) -> Result<Option<Lease>> {
            ensure!(!self.fail_prepare, "switch failed");
            Ok(self.lease.clone())
        }
        fn current(&mut self) -> Result<Source> {
            Ok(self.current.clone())
        }
        fn select_temporary(&mut self, _: &Lease) -> Result<()> {
            ensure!(!self.fail_select, "selection result uncertain");
            Ok(())
        }
        fn restore(&mut self, lease: &Lease) -> Result<Restoration> {
            self.restored = true;
            ensure!(!self.fail_restore, "restore failed");
            Ok(if self.current.same_selection(&lease.temporary) {
                Restoration::Restored(lease.original.clone())
            } else {
                Restoration::UserChanged
            })
        }
    }
    fn fake() -> Fake {
        let temporary = source("ascii", false);
        Fake {
            lease: Some(Lease {
                original: source("kanade", true),
                temporary: temporary.clone(),
            }),
            current: temporary,
            restored: false,
            fail_prepare: false,
            fail_select: false,
            fail_restore: false,
        }
    }
    #[test]
    fn keeps_ascii_after_success_and_attempts_restoration_after_failure() {
        let mut backend = fake();
        assert_eq!(transaction(&mut backend, || Ok(42)).unwrap(), 42);
        assert!(!backend.restored);
        backend.restored = false;
        assert!(transaction::<_, ()>(&mut backend, || bail!("update failed")).is_err());
        assert!(backend.restored);
    }
    #[test]
    fn failed_or_changed_selection_prevents_update() {
        let mut backend = fake();
        backend.fail_prepare = true;
        assert!(transaction(&mut backend, || -> Result<()> { panic!("must not update") }).is_err());
        assert!(!backend.restored);
        backend.fail_prepare = false;
        backend.current = source("different", false);
        assert!(transaction(&mut backend, || -> Result<()> { panic!("must not update") }).is_err());
        assert!(backend.restored);
        assert_eq!(backend.current.id, "different");
    }
    #[test]
    fn uncertain_selection_and_unwinding_attempt_restoration() {
        let mut backend = fake();
        backend.fail_select = true;
        assert!(transaction(&mut backend, || -> Result<()> { panic!("must not update") }).is_err());
        assert!(backend.restored);
        backend.fail_select = false;
        backend.restored = false;
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<()> = transaction(&mut backend, || panic!("interrupted update"));
        }));
        assert!(panic.is_err());
        assert!(backend.restored);
    }
    #[test]
    fn other_source_is_untouched_and_restore_failure_is_reported() {
        let mut backend = fake();
        backend.lease = None;
        transaction(&mut backend, || Ok(())).unwrap();
        assert!(!backend.restored);
        let mut backend = fake();
        backend.fail_restore = true;
        assert!(transaction::<_, ()>(&mut backend, || bail!("update failed")).is_err());
    }
    #[test]
    fn parent_resolves_only_to_an_enabled_selectable_child() {
        let mut parent = source("parent", true);
        parent.selectable = false;
        let mut child = source("child", true);
        child.mode = Some("com.apple.inputmethod.Japanese".into());
        assert_eq!(
            restore_index(&[parent.clone(), child.clone()], &parent).unwrap(),
            1
        );
        let mut disabled = child.clone();
        disabled.enabled = false;
        assert!(restore_index(&[parent.clone(), disabled], &parent).is_err());
        let mut another = child.clone();
        another.id = "another".into();
        assert!(restore_index(&[child, another], &parent).is_err());
    }
}
