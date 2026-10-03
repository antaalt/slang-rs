use crate as slang;

#[test]
fn compile() {
	let global_session = slang::GlobalSession::new().unwrap();

	let search_path = std::ffi::CString::new("shaders").unwrap();

	// All compiler options are available through this builder.
	let session_options = slang::CompilerOptions::default()
		.optimization(slang::OptimizationLevel::High)
		.matrix_layout_row(true);

	let target_desc = slang::TargetDesc::default()
		.format(slang::CompileTarget::Spirv)
		.profile(global_session.find_profile("glsl_450"));

	let targets = [target_desc];
	let search_paths = [search_path.as_ptr()];

	let file_system = slang::FileSystem::new(|path| std::fs::read(path).ok().map(slang::Blob::from));

	let session_desc = slang::SessionDesc::default()
		.targets(&targets)
		.search_paths(&search_paths)
		.options(&session_options)
		.file_system(&file_system);

	let session = global_session.create_session(&session_desc).unwrap();
	let module = session.load_module("test.slang").unwrap();
	let entry_point = module.find_entry_point_by_name("main").unwrap();

	let program = session
		.create_composite_component_type(&[module.into(), entry_point.into()])
		.unwrap();

	let linked_program = program.link().unwrap();

	// Entry point to the reflection API.
	let reflection = linked_program.layout(0).unwrap();
	assert_eq!(reflection.entry_point_count(), 1);
	assert_eq!(reflection.parameter_count(), 3);

	let shader_bytecode = linked_program.entry_point_code(0, 0).unwrap();
	assert_ne!(shader_bytecode.as_slice().len(), 0);
}

#[test]
fn lambda_file_system() {
	let global_session = slang::GlobalSession::new().unwrap();

	let target_desc = slang::TargetDesc::default()
		.format(slang::CompileTarget::Spirv)
		.profile(global_session.find_profile("glsl_450"));
	let targets = [target_desc];

	let source = "[shader(\"compute\")] [numthreads(1, 1, 1)] void main() {}";
	let file_system = slang::FileSystem::new(move |path| {
		(path == "virtual.slang").then(|| slang::Blob::from(source))
	});

	let blob = file_system.load_file("virtual.slang").unwrap();
	assert_eq!(blob.as_str().unwrap(), source);
	assert!(file_system.load_file("missing.slang").is_err());

	let session_desc = slang::SessionDesc::default()
		.targets(&targets)
		.file_system(&file_system);

	let session = global_session.create_session(&session_desc).unwrap();
	let module = session.load_module("virtual.slang").unwrap();
	let entry_point = module.find_entry_point_by_name("main").unwrap();

	let program = session
		.create_composite_component_type(&[module.into(), entry_point.into()])
		.unwrap();
	let linked_program = program.link().unwrap();

	let shader_bytecode = linked_program.entry_point_code(0, 0).unwrap();
	assert_ne!(shader_bytecode.as_slice().len(), 0);
}
