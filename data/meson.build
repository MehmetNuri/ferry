desktop_file = i18n.merge_file(
  input: '@0@.desktop.in'.format(app_id),
  output: '@0@.desktop'.format(app_id),
  type: 'desktop',
  po_dir: '../po',
  install: true,
  install_dir: get_option('datadir') / 'applications',
)

desktop_utils = find_program('desktop-file-validate', required: false)
if desktop_utils.found()
  test('Validate desktop file', desktop_utils, args: [desktop_file])
endif

metainfo_file = i18n.merge_file(
  input: '@0@.metainfo.xml.in'.format(app_id),
  output: '@0@.metainfo.xml'.format(app_id),
  po_dir: '../po',
  install: true,
  install_dir: get_option('datadir') / 'metainfo',
)

appstreamcli = find_program('appstreamcli', required: false)
if appstreamcli.found()
  test('Validate metainfo file', appstreamcli, args: ['validate', '--no-net', '--explain', metainfo_file])
endif

install_data('@0@.gschema.xml'.format(app_id), install_dir: get_option('datadir') / 'glib-2.0' / 'schemas')

compile_schemas = find_program('glib-compile-schemas', required: false)
if compile_schemas.found()
  test('Validate schema file', compile_schemas, args: ['--strict', '--dry-run', meson.current_source_dir()])
endif

install_data('icons/hicolor/scalable/apps/@0@.svg'.format(app_id), install_dir: get_option('datadir') / 'icons' / 'hicolor' / 'scalable' / 'apps')
install_data('icons/hicolor/symbolic/apps/@0@-symbolic.svg'.format(app_id), install_dir: get_option('datadir') / 'icons' / 'hicolor' / 'symbolic' / 'apps')

install_data('@0@.search-provider.ini'.format(app_id), install_dir: get_option('datadir') / 'gnome-shell' / 'search-providers')

service_conf = configuration_data()
service_conf.set('bindir', get_option('prefix') / get_option('bindir'))
configure_file(
  input: '@0@.service.in'.format(app_id),
  output: '@0@.service'.format(app_id),
  configuration: service_conf,
  install_dir: get_option('datadir') / 'dbus-1' / 'services',
)
