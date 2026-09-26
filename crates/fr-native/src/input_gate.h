#ifndef FR_INPUT_GATE_H
#define FR_INPUT_GATE_H
/* One same-user/local-X-server exclusion gate. No input or credentials stored.
 * Independent opens are independent kernel lock owners. */
int fr_input_gate_open(const char *display);
/* 1 acquired, 0 busy, -1 unavailable. Always nonblocking. */
int fr_input_gate_lock(int fd, int exclusive);
int fr_input_gate_unlock(int fd);
void fr_input_gate_close(int fd);
#endif
