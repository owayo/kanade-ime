//! MSI custom action. Only the installed supervisor runs at logon.
use anyhow::{Result, ensure};
use std::{os::windows::ffi::OsStrExt, path::Path};
use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, VARIANT_FALSE},
        System::{Com::*, TaskScheduler::*, Variant::VARIANT},
    },
    core::{BSTR, Interface},
};

const NAME: &str = "Kanade IME";
// The built-in Users group, also used by the Windows TSF logon task.
const USERS: &str = "S-1-5-32-545";

fn definition(service: &ITaskService, engine: &Path) -> Result<ITaskDefinition> {
    ensure!(engine.is_absolute(), "expected an absolute engine path");
    let path = BSTR::from_wide(&engine.as_os_str().encode_wide().collect::<Vec<_>>());
    unsafe {
        let task = service.NewTask(0)?;
        let principal = task.Principal()?;
        principal.SetGroupId(&USERS.into())?;
        principal.SetRunLevel(TASK_RUNLEVEL_LUA)?;
        task.Triggers()?.Create(TASK_TRIGGER_LOGON)?;
        let settings = task.Settings()?;
        // Each logon has its own endpoint and supervisor mutex.
        settings.SetMultipleInstances(TASK_INSTANCES_PARALLEL)?;
        settings.SetDisallowStartIfOnBatteries(VARIANT_FALSE)?;
        settings.SetStopIfGoingOnBatteries(VARIANT_FALSE)?;
        settings.SetExecutionTimeLimit(&"PT0S".into())?;
        settings.SetPriority(5)?; // Normal priority for interactive input.
        let action: IExecAction = task.Actions()?.Create(TASK_ACTION_EXEC)?.cast()?;
        action.SetPath(&path)?;
        action.SetArguments(&"--supervise".into())?;
        Ok(task)
    }
}

fn with_scheduler(operation: impl FnOnce(&ITaskService) -> Result<()>) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let result = (|| unsafe {
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
        let empty = VARIANT::default();
        service.Connect(&empty, &empty, &empty, &empty)?;
        operation(&service)
    })();
    unsafe { CoUninitialize() };
    result
}

pub(super) fn configure(engine: Option<&Path>) -> Result<()> {
    with_scheduler(|service| unsafe {
        let root = service.GetFolder(&r"\".into())?;
        if let Some(engine) = engine {
            let task = definition(service, engine)?;
            root.RegisterTaskDefinition(
                &NAME.into(),
                &task,
                TASK_CREATE_OR_UPDATE.0 | TASK_DONT_ADD_PRINCIPAL_ACE.0,
                &VARIANT::from(USERS),
                &VARIANT::default(),
                TASK_LOGON_GROUP,
                // Only administrators and SYSTEM may change what other users run.
                &VARIANT::from("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFX;;;BU)"),
            )?;
        } else if let Err(error) = root.DeleteTask(&NAME.into(), 0)
            && error.code() != ERROR_FILE_NOT_FOUND.to_hresult()
        {
            return Err(error.into());
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_round_trips_the_logon_definition_without_installing_it() -> Result<()> {
        // An installation path can contain spaces and XML metacharacters.
        let path = Path::new(r"C:\Program Files\IME & Tools\kanade-engine.exe");
        with_scheduler(|service| unsafe {
            let mut xml = BSTR::new();
            definition(service, path)?.XmlText(&mut xml)?;
            let task = service.NewTask(0)?;
            task.SetXmlText(&xml)?;
            let mut trigger = TASK_TRIGGER_TYPE2::default();
            task.Triggers()?.get_Item(1)?.Type(&mut trigger)?;
            assert_eq!(trigger, TASK_TRIGGER_LOGON);
            let action: IExecAction = task.Actions()?.get_Item(1)?.cast()?;
            let mut program = BSTR::new();
            action.Path(&mut program)?;
            assert_eq!(program.to_string(), path.to_string_lossy());
            Ok(())
        })
    }
}
