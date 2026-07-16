/*
 * Heya's macOS and Windows runtime libmpv loader.
 *
 * This MIT-licensed shim deliberately contains no MPV code and links no MPV
 * library into Heya. It exports the small libmpv surface used by our Rust
 * adapter, then resolves those functions from a user-installed dylib when MPV
 * is first requested. A failed load is retryable so Settings > Check again can
 * discover an installation made while Heya is running.
 */

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <wchar.h>
#else
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#endif

#define MPV_ERROR_UNINITIALIZED (-3)

typedef struct mpv_handle mpv_handle;
typedef struct mpv_render_context mpv_render_context;
typedef int mpv_format;
typedef int mpv_event_id;
typedef int mpv_render_param_type;

typedef struct mpv_event {
  mpv_event_id event_id;
  int error;
  uint64_t reply_userdata;
  void *data;
} mpv_event;

typedef struct mpv_render_param {
  mpv_render_param_type type;
  void *data;
} mpv_render_param;

typedef void (*mpv_render_update_fn)(void *callback_context);

typedef struct heya_mpv_api {
  unsigned long (*client_api_version)(void);
  mpv_handle *(*create)(void);
  mpv_handle *(*create_client)(mpv_handle *, const char *);
  int (*initialize)(mpv_handle *);
  void (*destroy)(mpv_handle *);
  void (*terminate_destroy)(mpv_handle *);
  void (*free_data)(void *);
  int (*load_config_file)(mpv_handle *, const char *);
  int64_t (*get_time_ns)(mpv_handle *);
  int64_t (*get_time_us)(mpv_handle *);
  int (*set_option)(mpv_handle *, const char *, mpv_format, void *);
  int (*command)(mpv_handle *, const char **);
  int (*set_property)(mpv_handle *, const char *, mpv_format, void *);
  int (*set_property_async)(mpv_handle *, uint64_t, const char *, mpv_format,
                            void *);
  int (*get_property)(mpv_handle *, const char *, mpv_format, void *);
  int (*observe_property)(mpv_handle *, uint64_t, const char *, mpv_format);
  int (*unobserve_property)(mpv_handle *, uint64_t);
  int (*request_event)(mpv_handle *, mpv_event_id, int);
  mpv_event *(*wait_event)(mpv_handle *, double);
  int (*render_context_create)(mpv_render_context **, mpv_handle *,
                               mpv_render_param *);
  void (*render_context_free)(mpv_render_context *);
  int (*render_context_get_info)(mpv_render_context *, mpv_render_param);
  int (*render_context_render)(mpv_render_context *, mpv_render_param *);
  void (*render_context_report_swap)(mpv_render_context *);
  void (*render_context_set_update_callback)(mpv_render_context *,
                                             mpv_render_update_fn, void *);
  uint64_t (*render_context_update)(mpv_render_context *);
} heya_mpv_api;

static heya_mpv_api API;
#ifdef _WIN32
static HMODULE LIBRARY_HANDLE;
static SRWLOCK LOAD_MUTEX = SRWLOCK_INIT;
static volatile LONG LOADED = 0;
static wchar_t RUNTIME_PATH[32768];
#else
static void *LIBRARY_HANDLE;
static pthread_mutex_t LOAD_MUTEX = PTHREAD_MUTEX_INITIALIZER;
static atomic_bool LOADED = false;
#endif

static int runtime_disabled(void) {
#ifdef _WIN32
  wchar_t value[8] = {0};
  DWORD length = GetEnvironmentVariableW(L"HEYA_LIBMPV_DISABLE", value, 8);
  return length == 1 && value[0] == L'1';
#else
  const char *value = getenv("HEYA_LIBMPV_DISABLE");
  return value && strcmp(value, "1") == 0;
#endif
}

static int resolve_api(
#ifdef _WIN32
    HMODULE handle,
#else
    void *handle,
#endif
    heya_mpv_api *api) {
#ifdef _WIN32
#define RESOLVE(field, symbol)                                                   \
  do {                                                                           \
    *(FARPROC *)(&api->field) = GetProcAddress(handle, symbol);                  \
    if (!api->field)                                                              \
      return 0;                                                                   \
  } while (0)
#else
#define RESOLVE(field, symbol)                                                   \
  do {                                                                           \
    *(void **)(&api->field) = dlsym(handle, symbol);                             \
    if (!api->field)                                                              \
      return 0;                                                                   \
  } while (0)
#endif

  RESOLVE(client_api_version, "mpv_client_api_version");
  RESOLVE(create, "mpv_create");
  RESOLVE(create_client, "mpv_create_client");
  RESOLVE(initialize, "mpv_initialize");
  RESOLVE(destroy, "mpv_destroy");
  RESOLVE(terminate_destroy, "mpv_terminate_destroy");
  RESOLVE(free_data, "mpv_free");
  RESOLVE(load_config_file, "mpv_load_config_file");
  RESOLVE(get_time_ns, "mpv_get_time_ns");
  RESOLVE(get_time_us, "mpv_get_time_us");
  RESOLVE(set_option, "mpv_set_option");
  RESOLVE(command, "mpv_command");
  RESOLVE(set_property, "mpv_set_property");
  RESOLVE(set_property_async, "mpv_set_property_async");
  RESOLVE(get_property, "mpv_get_property");
  RESOLVE(observe_property, "mpv_observe_property");
  RESOLVE(unobserve_property, "mpv_unobserve_property");
  RESOLVE(request_event, "mpv_request_event");
  RESOLVE(wait_event, "mpv_wait_event");
  RESOLVE(render_context_create, "mpv_render_context_create");
  RESOLVE(render_context_free, "mpv_render_context_free");
  RESOLVE(render_context_get_info, "mpv_render_context_get_info");
  RESOLVE(render_context_render, "mpv_render_context_render");
  RESOLVE(render_context_report_swap, "mpv_render_context_report_swap");
  RESOLVE(render_context_set_update_callback,
          "mpv_render_context_set_update_callback");
  RESOLVE(render_context_update, "mpv_render_context_update");

#undef RESOLVE
  return 1;
}

#ifdef _WIN32
static int absolute_windows_path(const wchar_t *path) {
  if (!path || !path[0])
    return 0;
  if (path[0] == L'\\' && path[1] == L'\\')
    return 1;
  return ((path[0] >= L'A' && path[0] <= L'Z') ||
          (path[0] >= L'a' && path[0] <= L'z')) &&
         path[1] == L':' && (path[2] == L'\\' || path[2] == L'/');
}

static int try_path(const wchar_t *path) {
  if (!absolute_windows_path(path))
    return 0;

  /* Search the verified runtime directory and System32 only. */
  HMODULE handle = LoadLibraryExW(path, NULL, 0x00000100 | 0x00000800);
  if (!handle)
    return 0;

  heya_mpv_api candidate = {0};
  if (!resolve_api(handle, &candidate)) {
    FreeLibrary(handle);
    return 0;
  }

  API = candidate;
  LIBRARY_HANDLE = handle;
  InterlockedExchange(&LOADED, 1);
  return 1;
}

/* Called by Rust with Heya's app-data runtime before the first probe. */
int heya_mpv_set_runtime_path(const wchar_t *path) {
  if (!absolute_windows_path(path))
    return 0;

  AcquireSRWLockExclusive(&LOAD_MUTEX);
  if (InterlockedCompareExchange(&LOADED, 0, 0) == 0) {
    size_t length = wcslen(path);
    if (length >= (sizeof(RUNTIME_PATH) / sizeof(RUNTIME_PATH[0]))) {
      ReleaseSRWLockExclusive(&LOAD_MUTEX);
      return 0;
    }
    memcpy(RUNTIME_PATH, path, (length + 1) * sizeof(wchar_t));
  }
  ReleaseSRWLockExclusive(&LOAD_MUTEX);
  return 1;
}

static int ensure_loaded(void) {
  if (InterlockedCompareExchange(&LOADED, 0, 0) != 0)
    return 1;
  if (runtime_disabled())
    return 0;

  AcquireSRWLockExclusive(&LOAD_MUTEX);
  if (InterlockedCompareExchange(&LOADED, 0, 0) == 0) {
    wchar_t override_path[32768] = {0};
    DWORD override_length = GetEnvironmentVariableW(
        L"HEYA_LIBMPV_PATH", override_path,
        (DWORD)(sizeof(override_path) / sizeof(override_path[0])));
    if (override_length > 0 &&
        override_length < (sizeof(override_path) / sizeof(override_path[0])))
      try_path(override_path);
    if (InterlockedCompareExchange(&LOADED, 0, 0) == 0 && RUNTIME_PATH[0])
      try_path(RUNTIME_PATH);
  }
  int loaded = InterlockedCompareExchange(&LOADED, 0, 0) != 0;
  ReleaseSRWLockExclusive(&LOAD_MUTEX);
  return loaded;
}
#else
static int try_path(const char *path) {
  if (!path || path[0] != '/')
    return 0;

  void *handle = dlopen(path, RTLD_NOW | RTLD_LOCAL);
  if (!handle)
    return 0;

  heya_mpv_api candidate = {0};
  if (!resolve_api(handle, &candidate)) {
    dlclose(handle);
    return 0;
  }

  API = candidate;
  LIBRARY_HANDLE = handle;
  atomic_store_explicit(&LOADED, true, memory_order_release);
  return 1;
}

static int ensure_loaded(void) {
  if (atomic_load_explicit(&LOADED, memory_order_acquire))
    return 1;
  if (runtime_disabled())
    return 0;

  pthread_mutex_lock(&LOAD_MUTEX);
  if (!atomic_load_explicit(&LOADED, memory_order_relaxed)) {
    const char *override_path = getenv("HEYA_LIBMPV_PATH");
    if (override_path)
      try_path(override_path);
    const char *paths[] = {
        "/opt/homebrew/opt/mpv/lib/libmpv.2.dylib",
        "/usr/local/opt/mpv/lib/libmpv.2.dylib",
        "/opt/local/lib/libmpv.2.dylib",
        "/usr/local/lib/libmpv.2.dylib",
        NULL,
    };
    for (size_t index = 0; paths[index]; index++) {
      if (atomic_load_explicit(&LOADED, memory_order_relaxed))
        break;
      if (try_path(paths[index]))
        break;
    }
  }
  int loaded = atomic_load_explicit(&LOADED, memory_order_acquire);
  pthread_mutex_unlock(&LOAD_MUTEX);
  return loaded;
}
#endif

unsigned long mpv_client_api_version(void) {
  return ensure_loaded() ? API.client_api_version() : 0;
}

mpv_handle *mpv_create(void) {
  return ensure_loaded() ? API.create() : NULL;
}

mpv_handle *mpv_create_client(mpv_handle *ctx, const char *name) {
  return ensure_loaded() ? API.create_client(ctx, name) : NULL;
}

int mpv_initialize(mpv_handle *ctx) {
  return ensure_loaded() ? API.initialize(ctx) : MPV_ERROR_UNINITIALIZED;
}

void mpv_destroy(mpv_handle *ctx) {
  if (ensure_loaded())
    API.destroy(ctx);
}

void mpv_terminate_destroy(mpv_handle *ctx) {
  if (ensure_loaded())
    API.terminate_destroy(ctx);
}

void mpv_free(void *data) {
  if (ensure_loaded())
    API.free_data(data);
}

int mpv_load_config_file(mpv_handle *ctx, const char *filename) {
  return ensure_loaded() ? API.load_config_file(ctx, filename)
                         : MPV_ERROR_UNINITIALIZED;
}

int64_t mpv_get_time_ns(mpv_handle *ctx) {
  return ensure_loaded() ? API.get_time_ns(ctx) : 0;
}

int64_t mpv_get_time_us(mpv_handle *ctx) {
  return ensure_loaded() ? API.get_time_us(ctx) : 0;
}

int mpv_set_option(mpv_handle *ctx, const char *name, mpv_format format,
                   void *data) {
  return ensure_loaded() ? API.set_option(ctx, name, format, data)
                         : MPV_ERROR_UNINITIALIZED;
}

int mpv_command(mpv_handle *ctx, const char **args) {
  return ensure_loaded() ? API.command(ctx, args) : MPV_ERROR_UNINITIALIZED;
}

int mpv_set_property(mpv_handle *ctx, const char *name, mpv_format format,
                     void *data) {
  return ensure_loaded() ? API.set_property(ctx, name, format, data)
                         : MPV_ERROR_UNINITIALIZED;
}

int mpv_set_property_async(mpv_handle *ctx, uint64_t reply_userdata,
                           const char *name, mpv_format format, void *data) {
  return ensure_loaded()
             ? API.set_property_async(ctx, reply_userdata, name, format, data)
             : MPV_ERROR_UNINITIALIZED;
}

int mpv_get_property(mpv_handle *ctx, const char *name, mpv_format format,
                     void *data) {
  return ensure_loaded() ? API.get_property(ctx, name, format, data)
                         : MPV_ERROR_UNINITIALIZED;
}

int mpv_observe_property(mpv_handle *ctx, uint64_t reply_userdata,
                         const char *name, mpv_format format) {
  return ensure_loaded()
             ? API.observe_property(ctx, reply_userdata, name, format)
             : MPV_ERROR_UNINITIALIZED;
}

int mpv_unobserve_property(mpv_handle *ctx, uint64_t reply_userdata) {
  return ensure_loaded() ? API.unobserve_property(ctx, reply_userdata)
                         : MPV_ERROR_UNINITIALIZED;
}

int mpv_request_event(mpv_handle *ctx, mpv_event_id event, int enable) {
  return ensure_loaded() ? API.request_event(ctx, event, enable)
                         : MPV_ERROR_UNINITIALIZED;
}

mpv_event *mpv_wait_event(mpv_handle *ctx, double timeout) {
  static mpv_event unavailable_event = {0, MPV_ERROR_UNINITIALIZED, 0, NULL};
  return ensure_loaded() ? API.wait_event(ctx, timeout) : &unavailable_event;
}

int mpv_render_context_create(mpv_render_context **result, mpv_handle *mpv,
                              mpv_render_param *params) {
  return ensure_loaded() ? API.render_context_create(result, mpv, params)
                         : MPV_ERROR_UNINITIALIZED;
}

void mpv_render_context_free(mpv_render_context *ctx) {
  if (ensure_loaded())
    API.render_context_free(ctx);
}

int mpv_render_context_get_info(mpv_render_context *ctx,
                                mpv_render_param param) {
  return ensure_loaded() ? API.render_context_get_info(ctx, param)
                         : MPV_ERROR_UNINITIALIZED;
}

int mpv_render_context_render(mpv_render_context *ctx,
                              mpv_render_param *params) {
  return ensure_loaded() ? API.render_context_render(ctx, params)
                         : MPV_ERROR_UNINITIALIZED;
}

void mpv_render_context_report_swap(mpv_render_context *ctx) {
  if (ensure_loaded())
    API.render_context_report_swap(ctx);
}

void mpv_render_context_set_update_callback(mpv_render_context *ctx,
                                            mpv_render_update_fn callback,
                                            void *callback_context) {
  if (ensure_loaded())
    API.render_context_set_update_callback(ctx, callback, callback_context);
}

uint64_t mpv_render_context_update(mpv_render_context *ctx) {
  return ensure_loaded() ? API.render_context_update(ctx) : 0;
}
