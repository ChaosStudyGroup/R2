use super::*;
use apis_log::{LogErr, LogSyncHandler};
use std::fs::File;

pub struct LogApis<'p> {
    r2: Arc<Mutex<R2<'p>>>,
}

impl<'p> LogApis<'p> {
    pub fn new(r2: Arc<Mutex<R2>>) -> LogApis {
        LogApis { r2 }
    }
}

impl<'p> LogSyncHandler for LogApis<'p> {
    fn handle_show(&self, filename: String) -> thrift::Result<()> {
        let r2 = self.r2.lock().unwrap();
        for t in r2.threads.iter() {
            let name = format!("{}:{}", filename, t.thread);
            let file = match File::create(&name) {
                Err(why) => {
                    return Err(LogErr::new(format!(
                        "couldn't create {}: {}",
                        filename,
                        why.to_string()
                    )))
                    .map_err(From::from);
                }
                Ok(file) => file,
            };
            if let Err(why) = t.logger.serialize(file) {
                return Err(LogErr::new(format!(
                    "couldn't write log {}: {}",
                    name,
                    why.to_string()
                )))
                .map_err(From::from);
            }
        }
        Ok(())
    }
}
