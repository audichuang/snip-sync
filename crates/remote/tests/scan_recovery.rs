use std::sync::Arc;

use snip_remote::{Client, ErrorCode, RemoteHost, Worker};

#[test]
fn scan_eofs_close_real_streams_and_leave_open_and_list_usable() {
	let folder = tempfile::tempdir().unwrap();
	std::fs::write(folder.path().join("kept.txt"), "kept").unwrap();
	let worker = Arc::new(Worker::new(Default::default()));
	let client =
		Client::new(RemoteHost::in_process(worker.clone()), "test".into());
	let workspace = client
		.open_workspace(folder.path().to_str().unwrap())
		.unwrap();
	worker.set_scan_failures_for_tests(&workspace.id, 2, None);
	assert_eq!(client.list_dir(&workspace.id, "", None).unwrap().0.len(), 1);
	let error = client.scan_repos(&workspace.id, None, None).unwrap_err();
	assert!(error.is_transient_read_failure(), "{error:?}");
	assert_eq!(
		worker.scan_requests_for_tests(&workspace.id),
		2,
		"one reused read retry, then caller-owned backoff"
	);
	assert_eq!(worker.started_connections_for_tests(), 2);
	client.scan_repos(&workspace.id, None, None).unwrap();
	assert_eq!(worker.scan_requests_for_tests(&workspace.id), 3);
	assert_eq!(worker.started_connections_for_tests(), 3);
	assert_eq!(client.list_dir(&workspace.id, "", None).unwrap().0.len(), 1);
	worker.set_scan_failures_for_tests(
		&workspace.id,
		0,
		Some(ErrorCode::Forbidden),
	);
	let error = client.scan_repos(&workspace.id, None, None).unwrap_err();
	assert!(!error.is_transient_read_failure());
	assert_eq!(
		worker.scan_requests_for_tests(&workspace.id),
		1,
		"a permanent refusal is not retried"
	);
	assert_eq!(worker.started_connections_for_tests(), 3);
	client.open_workspace(&workspace.id).unwrap();
	assert_eq!(client.list_dir(&workspace.id, "", None).unwrap().0.len(), 1);
}
