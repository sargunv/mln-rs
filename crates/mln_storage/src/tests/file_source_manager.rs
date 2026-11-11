// Tests that file sources are shared within one FileSourceManager and never
// across managers.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mln_actor::RunLoop;

use super::serial;
use crate::file_source::{FileSource, FileSourceType};
use crate::file_source_manager::FileSourceManager;
use crate::online_file_source::OnlineFileSource;
use crate::resource::{Kind, Resource};
use crate::resource_options::ResourceOptions;
use crate::resource_transform::{FinishedCallback, ResourceTransform};
use crate::response::{Error, Reason, Response};

/// Registers a `Network` factory on `manager` that counts its calls.
fn count_network_factory(manager: &FileSourceManager) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    manager.register_file_source_factory(
        FileSourceType::Network,
        Arc::new(move |_: &FileSourceManager, options: &ResourceOptions| {
            counter.fetch_add(1, Ordering::SeqCst);
            Arc::new(OnlineFileSource::new(options)) as Arc<dyn FileSource>
        }),
    );
    calls
}

#[test]
fn shares_a_live_file_source_per_type_and_options() {
    let manager = FileSourceManager::new();
    let options = ResourceOptions::default();
    let first = manager.get_file_source(FileSourceType::Network, &options).unwrap();
    let second = manager.get_file_source(FileSourceType::Network, &options).unwrap();
    assert!(Arc::ptr_eq(&first, &second));

    let elsewhere =
        manager.get_file_source(FileSourceType::Network, &options.clone().with_cache_path("other")).unwrap();
    assert!(!Arc::ptr_eq(&first, &elsewhere));

    let clone = manager.clone();
    assert!(Arc::ptr_eq(&first, &clone.get_file_source(FileSourceType::Network, &options).unwrap()));
}

#[test]
fn separate_managers_never_share_a_file_source() {
    let options = ResourceOptions::default();
    let a = FileSourceManager::new().get_file_source(FileSourceType::Network, &options).unwrap();
    let b = FileSourceManager::new().get_file_source(FileSourceType::Network, &options).unwrap();
    assert!(!Arc::ptr_eq(&a, &b));
}

#[test]
fn a_released_file_source_is_made_again() {
    let manager = FileSourceManager::empty();
    let calls = count_network_factory(&manager);
    let options = ResourceOptions::default();

    let file_source = manager.get_file_source(FileSourceType::Network, &options).unwrap();
    let _shared = manager.get_file_source(FileSourceType::Network, &options).unwrap();
    assert_eq!(1, calls.load(Ordering::SeqCst));

    drop((file_source, _shared));
    let _again = manager.get_file_source(FileSourceType::Network, &options).unwrap();
    assert_eq!(2, calls.load(Ordering::SeqCst));
}

#[test]
fn a_factory_stays_with_its_manager() {
    let with_factory = FileSourceManager::new();
    let calls = count_network_factory(&with_factory);
    let other = FileSourceManager::new();
    let options = ResourceOptions::default();

    let _other_loader = other.get_file_source(FileSourceType::ResourceLoader, &options).unwrap();
    assert_eq!(0, calls.load(Ordering::SeqCst));

    // The ResourceLoader takes its network file source from its own manager.
    let _loader = with_factory.get_file_source(FileSourceType::ResourceLoader, &options).unwrap();
    assert_eq!(1, calls.load(Ordering::SeqCst));
}

#[test]
fn unregistered_types_have_no_file_source() {
    let manager = FileSourceManager::new();
    assert!(manager.unregister_file_source_factory(FileSourceType::Asset).is_some());
    assert!(manager.get_file_source(FileSourceType::Asset, &ResourceOptions::default()).is_none());
    assert!(FileSourceManager::empty().get_file_source(FileSourceType::Asset, &ResourceOptions::default()).is_none());
}

fn failing_transform(message: &'static str) -> ResourceTransform {
    ResourceTransform::new(Some(Arc::new(move |_kind: Kind, _url: &str, finished: FinishedCallback| {
        finished(Err(Error::new(Reason::Other, message)));
    })))
}

/// The error message of a request through `manager`'s ResourceLoader.
fn loader_error(manager: &FileSourceManager) -> String {
    let run_loop = RunLoop::new();
    let loader = manager.get_file_source(FileSourceType::ResourceLoader, &ResourceOptions::default()).unwrap();
    let message = Arc::new(std::sync::Mutex::new(String::new()));
    let (out, stop) = (message.clone(), run_loop.clone());
    let _req = loader.request(
        &Resource::new(Kind::Unknown, "http://127.0.0.1:3000/test"),
        Box::new(move |res: Response| {
            *out.lock().unwrap() = res.error.expect("error").message;
            stop.stop();
        }),
    );
    run_loop.run();
    message.lock().unwrap().clone()
}

#[test]
fn a_resource_transform_stays_with_its_manager() {
    let _serial = serial();
    let (a, b) = (FileSourceManager::new(), FileSourceManager::new());
    let options = ResourceOptions::default();
    let network_a = a.get_file_source(FileSourceType::Network, &options).unwrap();
    let network_b = b.get_file_source(FileSourceType::Network, &options).unwrap();
    network_a.set_resource_transform(failing_transform("transform a"));
    network_b.set_resource_transform(failing_transform("transform b"));

    assert_eq!("transform a", loader_error(&a));
    assert_eq!("transform b", loader_error(&b));
}
