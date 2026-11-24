//! Android devices and Logcat through the `ide-android` crate.
use crate::Host;
use crate::ops::Channel;
use crate::rpc::Dispatcher;
use ide_android::{AndroidDevice, LogcatLine};
use ide_core::{IdeResult, RequestId};

pub async fn devices(host: &Host) -> IdeResult<Vec<AndroidDevice>> {
    host.blocking(ide_android::devices).await
}

pub async fn logcat(
    host: &Host,
    window: &str,
    request: RequestId,
    serial: String,
    clear: bool,
    on_lines: Channel<Vec<LogcatLine>>,
) -> IdeResult<()> {
    let cancel = host.begin_project_read_request(window, request);
    let result = host
        .blocking(move || {
            ide_android::logcat(&serial, clear, &cancel, &mut |batch| {
                let _ = on_lines.send(batch);
            })
        })
        .await;
    host.end_request(request);
    result
}

pub(crate) fn register(d: &mut Dispatcher) {
    d.add("android_devices", |c| async move { devices(&c.host).await });
    d.add("android_logcat", |c| async move {
        logcat(&c.host, &c.window, c.arg("request")?, c.arg("serial")?, c.arg("clear")?, c.sender("onLines")?).await
    });
}
