project('ferry',
  version: '0.1.0',
  meson_version: '>= 1.0.0',
)

i18n = import('i18n')
gnome = import('gnome')

app_id = 'io.github.mehmetnuri.Ferry'
gettext_package = 'ferry'

subdir('data')
subdir('src')
subdir('po')

gnome.post_install(
  glib_compile_schemas: true,
  gtk_update_icon_cache: true,
  update_desktop_database: true,
)
