//! Same-user, on-demand Task Scheduler registration through its COM API.
//! PowerShell hosts the OS COM interface; it receives only non-secret metadata.
use crate::platform::{
    independent_spawn::check,
    process::{SpawnStdio, StdioSource, SyncEnvironment},
};
use std::{
    io,
    path::Path,
    process::Command,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

const CONTROL: &str = r#"param([string]$Operation,[string]$TaskName,[string]$Definition)
$ErrorActionPreference = 'Stop'
try {
  $service = New-Object -ComObject 'Schedule.Service'
  $service.Connect()
  $folder = $service.GetFolder('\')
  switch ($Operation) {
    'create' {
      $document = [xml]$Definition
      $now = [DateTime]::UtcNow
      $document.Task.Triggers.TimeTrigger.StartBoundary = $now.ToString('yyyy-MM-ddTHH:mm:ssZ')
      $document.Task.Triggers.TimeTrigger.EndBoundary = $now.AddSeconds(45).ToString('yyyy-MM-ddTHH:mm:ssZ')
      $sid = [string]$document.Task.Principals.Principal.UserId
      # COM transports a Unicode BSTR, not an encoded XML byte stream.
      $folder.RegisterTask($TaskName,$document.DocumentElement.OuterXml,2,$sid,$null,3,$null) | Out-Null
    }
    'run' { $folder.GetTask($TaskName).Run($null) | Out-Null }
    'end' { $folder.GetTask($TaskName).Stop(0) }
    'delete' { $folder.DeleteTask($TaskName,0) }
    'exists' { $folder.GetTask($TaskName) | Out-Null }
    default { exit 6 }
  }
  exit 0
} catch {
  $errorObject = $_.Exception
  while ($null -ne $errorObject.InnerException) { $errorObject = $errorObject.InnerException }
  $code = ([long]$errorObject.HResult) -band 4294967295L
  if ($code -eq 2147942405L) { exit 3 }
  if ($code -eq 2147942402L) { exit 5 }
  if ($code -eq 2147944122L -or $code -eq 2147746132L -or $code -eq 2147750677L) { exit 4 }
  exit $errorObject.HResult
}
"#;

pub(super) struct ScheduledTask {
    name: String,
    definition: String,
    registered: bool,
}

impl ScheduledTask {
    pub(super) fn prepare(helper: &Path) -> io::Result<(Self, String)> {
        if !helper.is_absolute() {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut entropy = [0_u8; 16];
        getrandom::fill(&mut entropy).map_err(io::Error::other)?;
        let nonce: String = entropy.iter().map(|byte| format!("{byte:02x}")).collect();
        let name = format!("rp-independent-{nonce}");
        let endpoint = format!(r"\\.\pipe\{name}");
        let definition = task_xml(helper, &endpoint, &super::ipc::current_user_sid_text()?)?;
        Ok((
            Self {
                name,
                definition,
                registered: false,
            },
            endpoint,
        ))
    }

    pub(super) fn start(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        // Arm before registration: a timeout may follow partial manager success.
        self.registered = true;
        self.command("create", deadline, cancelled)
            .map_err(|error| io::Error::new(error.kind(), format!("Task Scheduler create: {error}")))?;
        self.command("run", deadline, cancelled)
            .map_err(|error| io::Error::new(error.kind(), format!("Task Scheduler run: {error}")))
    }

    pub(super) fn remove_definition(
        &mut self,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<()> {
        self.command("delete", deadline, cancelled)?;
        self.registered = false;
        Ok(())
    }

    fn command(
        &self,
        operation: &str,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<()> {
        check(deadline, cancelled)?;
        // The control host needs the user's OS login environment even when the
        // requester has a deliberately minimal environment. Do not pass target
        // environment entries to PowerShell; those travel only over private IPC.
        let environment = super::host::login_environment()?;
        let system_root = environment
            .iter()
            .find(|(key, _)| {
                key.to_str()
                    .is_some_and(|key| key.eq_ignore_ascii_case("SystemRoot"))
            })
            .map(|(_, value)| value)
            .map(std::path::PathBuf::from)
            .filter(|root| root.is_absolute())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::Unsupported, "absolute SystemRoot unavailable")
            })?;
        let mut command = Command::new(
            system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
        );
        let literal = |value: &str| format!("'{}'", value.replace('\'', "''"));
        let script = format!(
            "& {{ {CONTROL} }} -Operation {} -TaskName {} -Definition {}",
            literal(operation),
            literal(&self.name),
            literal(&self.definition)
        );
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ]);
        let stdio = SpawnStdio {
            stdout: StdioSource::Null,
            stderr: StdioSource::Null,
            drain_timeout: None,
            ..SpawnStdio::default()
        };
        let mut child =
            crate::spawn_sync(&mut command, stdio, SyncEnvironment::Explicit(environment)).map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Task Scheduler COM host unavailable",
                    )
                } else {
                    error
                }
            })?;
        loop {
            check(deadline, cancelled)?;
            if let Some(code) = child.try_wait()? {
                return match code {
                    0 => Ok(()),
                    3 => Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                    4 => Err(io::Error::from(io::ErrorKind::Unsupported)),
                    5 => Err(io::Error::from(io::ErrorKind::NotFound)),
                    _ => Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("Task Scheduler {operation} failed (HRESULT 0x{:08x})", code as u32),
                    )),
                };
            }
            std::thread::sleep(
                Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

impl Drop for ScheduledTask {
    fn drop(&mut self) {
        if self.registered {
            let cancelled = AtomicBool::new(false);
            let _ = self.command("end", Instant::now() + Duration::from_secs(2), &cancelled);
            let _ = self.command(
                "delete",
                Instant::now() + Duration::from_secs(2),
                &cancelled,
            );
        }
    }
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn task_xml(helper: &Path, endpoint: &str, sid: &str) -> io::Result<String> {
    let helper = helper
        .to_str()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    // The disabled trigger cannot launch anything. Its end boundary exists
    // only to let the service expire registration even if the requester dies
    // before Run or Drop. CONTROL stamps the boundaries immediately before
    // registration; normal successful launches remove the definition sooner.
    Ok(format!(
        r#"<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<RegistrationInfo><Description>running-process independent launcher</Description></RegistrationInfo>
<Triggers><TimeTrigger><StartBoundary>2000-01-01T00:00:00Z</StartBoundary><EndBoundary>2000-01-01T00:00:45Z</EndBoundary><Enabled>false</Enabled></TimeTrigger></Triggers>
<Principals><Principal id="Caller"><UserId>{}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><DeleteExpiredTaskAfter>PT1S</DeleteExpiredTaskAfter></Settings>
<Actions Context="Caller"><Exec><Command>{}</Command><Arguments>&quot;{}&quot;</Arguments></Exec></Actions>
</Task>"#,
        escape(sid),
        escape(helper),
        escape(endpoint)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_is_on_demand_same_user_and_not_elevated() {
        let xml = task_xml(
            Path::new(r"C:\Program Files\helper.exe"),
            r"\\.\pipe\fixture",
            "S-1-5-21-123",
        )
        .unwrap();
        assert!(xml.contains("<Enabled>false</Enabled></TimeTrigger>"));
        assert!(xml.contains("<DeleteExpiredTaskAfter>PT1S</DeleteExpiredTaskAfter>"));
        assert!(!xml.contains("<BootTrigger"));
        assert!(!xml.contains("<LogonTrigger"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("S-1-5-21-123"));
        assert!(!xml.contains("HighestAvailable"));
    }

    #[test]
    #[ignore = "requires real Task Scheduler; verifies registration-only crash cleanup"]
    fn abandoned_registration_expires_without_running() {
        let (mut task, _) = ScheduledTask::prepare(&std::env::current_exe().unwrap()).unwrap();
        let cancelled = AtomicBool::new(false);
        task.registered = true;
        task.command("create", Instant::now() + Duration::from_secs(15), &cancelled).unwrap();
        // No Run and no Drop: reproduce the registration-before-launch crash
        // window. Keep a guard solely to clean up if the assertion fails.
        let deadline = Instant::now() + Duration::from_secs(100);
        loop {
            match task.command("exists", deadline, &cancelled) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => panic!("expiration query failed: {error}"),
                Ok(()) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        task.registered = false;
    }
}
