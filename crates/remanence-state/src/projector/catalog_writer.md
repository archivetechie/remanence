
Writer privacy: private constructor.
```no_run
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = projector::open(path)?.write_handle();
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```
```compile_fail
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = CatalogIndex::open(path)?;
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```

Writer privacy: reader mutation.
```no_run
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = projector::open(path)?.write_handle();
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```
```compile_fail
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = CatalogReader::open(path)?;
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```

Writer privacy: state mutable escape.
```no_run
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = state.write_handle();
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```
```compile_fail
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let writer: &mut CatalogIndex = state.catalog_index();
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```

Writer privacy: private fields.
```no_run
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = projector::open(path)?.write_handle();
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```
```compile_fail
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = CatalogIndex {};
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```

Writer privacy: no Default.
```no_run
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = projector::open(path)?.write_handle();
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```
```compile_fail
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer: CatalogIndex = Default::default();
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```

Writer privacy: no Clone.
```no_run
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = projector::open(path)?.write_handle();
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```
```compile_fail
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = projector::open(path)?.write_handle().clone();
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```

Writer privacy: no reader conversion.
```no_run
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = projector::open(path)?.write_handle();
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```
```compile_fail
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = CatalogIndex::from(CatalogReader::open(path)?);
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```

Writer privacy: no writable read-only open.
```no_run
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = projector::open(path)?.write_handle();
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```
```compile_fail
use remanence_state::{projector, CatalogIndex, CatalogReader, StateHandle, StateError};
fn example(path: &std::path::Path, mut state: StateHandle) -> Result<(), StateError> {
    let mut writer = CatalogIndex::open_read_only(path)?;
    writer.set_drive_fenced(&[0; 16], true)?;
    Ok(())
}
```

Streaming visitors run only on independent readers, so caller code cannot hold
up the shared writer connection.
```no_run
use remanence_state::{CatalogReader, StateError};
use remanence_state::index::CatalogUnitFilter;
use std::ops::ControlFlow;
fn example(path: &std::path::Path) -> Result<(), StateError> {
    let reader = CatalogReader::open(path)?;
    reader.for_each_catalog_unit(CatalogUnitFilter::All, |_| ControlFlow::Continue(()))?;
    reader.for_each_native_object(|_| ControlFlow::Continue(()))?;
    Ok(())
}
```
```compile_fail
use remanence_state::{CatalogIndex, StateError};
use remanence_state::index::CatalogUnitFilter;
use std::ops::ControlFlow;
fn example(writer: &CatalogIndex) -> Result<(), StateError> {
    writer.for_each_catalog_unit(CatalogUnitFilter::All, |_| ControlFlow::Continue(()))
}
```
```compile_fail
use remanence_state::{CatalogIndex, StateError};
use std::ops::ControlFlow;
fn example(writer: &CatalogIndex) -> Result<(), StateError> {
    writer.for_each_native_object(|_| ControlFlow::Continue(()))
}
```
