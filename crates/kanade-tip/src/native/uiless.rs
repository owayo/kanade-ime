use super::*;
use windows::core::BSTR;
#[implement(ITfCandidateListUIElementBehavior)]
pub(super) struct Element {
    shared: Weak<Shared>,
    _guard: ModuleGuard,
}
impl Element {
    pub fn new(shared: &Rc<Shared>) -> Self {
        Self {
            shared: Rc::downgrade(shared),
            _guard: ModuleGuard::new(),
        }
    }
}
impl ITfUIElement_Impl for Element_Impl {
    fn GetDescription(&self) -> Result<BSTR> {
        Ok("Kanade candidates".into())
    }
    fn GetGUID(&self) -> Result<GUID> {
        Ok(GUID::from_u128(0x0e4ba76b_d2c4_4080_aa0c_7acd3d67a356))
    }
    fn Show(&self, show: BOOL) -> Result<()> {
        if let Some(s) = self.shared.upgrade() {
            s.ui_native.set(show.as_bool());
            if !show.as_bool()
                && let Ok(window) = s.window.try_borrow()
                && let Some(window) = window.as_ref()
            {
                window.hide();
            }
        }
        Ok(())
    }
    fn IsShown(&self) -> Result<BOOL> {
        Ok(self
            .shared
            .upgrade()
            .is_some_and(|s| s.ui_native.get())
            .into())
    }
}
impl ITfCandidateListUIElement_Impl for Element_Impl {
    fn GetUpdatedFlags(&self) -> Result<u32> {
        Ok(0x3f)
    }
    fn GetDocumentMgr(&self) -> Result<ITfDocumentMgr> {
        let shared = self.shared.upgrade().ok_or_else(failure)?;
        let context = shared
            .context
            .try_borrow()
            .ok()
            .and_then(|c| c.clone())
            .ok_or_else(failure)?;
        unsafe { context.GetDocumentMgr() }
    }
    fn GetCount(&self) -> Result<u32> {
        let shared = self.shared.upgrade().ok_or_else(failure)?;
        let page = shared.page.try_borrow().map_err(|_| failure())?;
        Ok(page.items.len() as u32)
    }
    fn GetSelection(&self) -> Result<u32> {
        Ok(self
            .shared
            .upgrade()
            .ok_or_else(failure)?
            .ui_selection
            .get())
    }
    fn GetString(&self, index: u32) -> Result<BSTR> {
        let shared = self.shared.upgrade().ok_or_else(failure)?;
        let page = shared.page.try_borrow().map_err(|_| failure())?;
        Ok(page
            .items
            .get(index as usize)
            .ok_or_else(|| Error::from(E_INVALIDARG))?
            .value
            .as_str()
            .into())
    }
    fn GetPageIndex(&self, index: *mut u32, size: u32, count: *mut u32) -> Result<()> {
        if count.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *count = 1;
            if size > 0 {
                if index.is_null() {
                    return Err(E_POINTER.into());
                }
                *index = 0;
            }
        }
        Ok(())
    }
    fn SetPageIndex(&self, index: *const u32, count: u32) -> Result<()> {
        if count != 1 || index.is_null() || unsafe { *index } != 0 {
            return Err(E_INVALIDARG.into());
        }
        Ok(())
    }
    fn GetCurrentPage(&self) -> Result<u32> {
        Ok(0)
    }
}
impl ITfCandidateListUIElementBehavior_Impl for Element_Impl {
    fn SetSelection(&self, index: u32) -> Result<()> {
        let shared = self.shared.upgrade().ok_or_else(failure)?;
        shared.stop_refinement();
        if shared
            .page
            .try_borrow()
            .map_err(|_| failure())?
            .items
            .get(index as usize)
            .is_none()
        {
            return Err(E_INVALIDARG.into());
        }
        shared.ui_selection.set(index);
        Ok(())
    }
    fn Finalize(&self) -> Result<()> {
        let shared = self.shared.upgrade().ok_or_else(failure)?;
        let handle = shared
            .page
            .try_borrow()
            .ok()
            .and_then(|p| {
                p.items
                    .get(shared.ui_selection.get() as usize)
                    .and_then(|c| c.handle)
            })
            .ok_or_else(failure)?;
        shared.choose(handle);
        Ok(())
    }
    fn Abort(&self) -> Result<()> {
        if let Some(s) = self.shared.upgrade() {
            s.reset();
        }
        Ok(())
    }
}
