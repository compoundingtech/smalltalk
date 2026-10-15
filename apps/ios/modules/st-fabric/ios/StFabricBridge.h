#ifndef ST_FABRIC_BRIDGE_H
#define ST_FABRIC_BRIDGE_H
#include <stddef.h>
#include <stdint.h>
char *st_fabric_identity(const uint8_t *secret, size_t length);
char *st_fabric_dial(const uint8_t *secret, size_t length, const char *target_json);
char *st_fabric_open_target(const uint8_t *secret, size_t length, const char *target_json);
char *st_fabric_close_target(const char *handle);
char *st_fabric_target_stats(const char *handle);
size_t st_fabric_target_count(void);
char *st_fabric_stop(void);
char *st_fabric_stop_all(void);
char *st_fabric_stats(void);
char *st_fabric_network_change(void);
void st_fabric_string_free(char *reply);
#endif
