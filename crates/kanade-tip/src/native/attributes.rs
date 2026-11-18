use super::*;
use windows::core::BSTR;
pub const ATTRIBUTE: GUID = GUID::from_u128(0x4807f46d_d513_49a1_b9d7_bdd864243d46);
#[implement(ITfDisplayAttributeInfo)]
pub struct Attribute {
    _guard: ModuleGuard,
}
impl Attribute {
    pub fn new() -> Self {
        Self {
            _guard: ModuleGuard::new(),
        }
    }
}
impl ITfDisplayAttributeInfo_Impl for Attribute_Impl {
    fn GetGUID(&self) -> Result<GUID> {
        Ok(ATTRIBUTE)
    }
    fn GetDescription(&self) -> Result<BSTR> {
        Ok("Kanade composition".into())
    }
    fn GetAttributeInfo(&self, out: *mut TF_DISPLAYATTRIBUTE) -> Result<()> {
        if out.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *out = TF_DISPLAYATTRIBUTE {
                lsStyle: TF_LS_SOLID,
                bAttr: TF_ATTR_INPUT,
                ..Default::default()
            };
        }
        Ok(())
    }
    fn SetAttributeInfo(&self, _: *const TF_DISPLAYATTRIBUTE) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Reset(&self) -> Result<()> {
        Ok(())
    }
}
#[implement(IEnumTfDisplayAttributeInfo)]
pub struct Enumerator {
    done: Cell<bool>,
    _guard: ModuleGuard,
}
impl Enumerator {
    pub fn new() -> Self {
        Self {
            done: Cell::new(false),
            _guard: ModuleGuard::new(),
        }
    }
}
impl IEnumTfDisplayAttributeInfo_Impl for Enumerator_Impl {
    fn Clone(&self) -> Result<IEnumTfDisplayAttributeInfo> {
        Ok(Enumerator {
            done: Cell::new(self.done.get()),
            _guard: ModuleGuard::new(),
        }
        .into())
    }
    fn Next(
        &self,
        count: u32,
        out: *mut Option<ITfDisplayAttributeInfo>,
        fetched: *mut u32,
    ) -> Result<()> {
        if out.is_null() || fetched.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *fetched = 0;
        }
        if count > 0 && !self.done.replace(true) {
            unsafe {
                *out = Some(Attribute::new().into());
                *fetched = 1;
            }
            if count == 1 {
                return Ok(());
            }
        }
        Err(Error::from_hresult(S_FALSE))
    }
    fn Reset(&self) -> Result<()> {
        self.done.set(false);
        Ok(())
    }
    fn Skip(&self, count: u32) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        if count == 1 && !self.done.replace(true) {
            Ok(())
        } else {
            Err(Error::from_hresult(S_FALSE))
        }
    }
}
