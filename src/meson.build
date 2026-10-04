cargo = find_program('cargo')
cargo_options = ['--manifest-path', meson.project_source_root() / 'Cargo.toml', '--target-dir', meson.project_build_root() / 'target']
if get_option('profile') == 'default'
  cargo_options += ['--release']
  rust_target = 'release'
else
  rust_target = 'debug'
endif

# Highlighting is a default feature; it is left out when GtkSourceView 5 is missing.
if not dependency('gtksourceview-5', required: get_option('sourceview')).found()
  cargo_options += ['--no-default-features']
endif

cargo_env = ['FERRY_LOCALEDIR=' + get_option('prefix') / get_option('localedir')]

custom_target(
  'cargo-build',
  build_by_default: true,
  build_always_stale: true,
  output: meson.project_name(),
  console: true,
  install: true,
  install_dir: get_option('bindir'),
  command: [
    'env', cargo_env,
    cargo, 'build', cargo_options,
    '&&', 'cp', meson.project_build_root() / 'target' / rust_target / meson.project_name(), '@OUTPUT@',
  ],
)
