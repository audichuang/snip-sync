/* Test-only XCB shim: deliver CONNECT_REPLY after the driver clicks an input.
 * It neither edits nor suppresses any protocol request. Only the app loads it.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <xcb/xcb.h>
#include <xcb/xcbext.h>

static xcb_connection_t *xim_connection;
static xcb_atom_t protocol_atom, connect_atom;
static xcb_window_t server_window, client_window;

static xcb_atom_t atom(xcb_connection_t *connection, const char *name) {
    xcb_intern_atom_reply_t *reply = xcb_intern_atom_reply(
        connection, xcb_intern_atom(connection, 1, strlen(name), name), NULL);
    xcb_atom_t value = reply ? reply->atom : XCB_ATOM_NONE;
    free(reply);
    return value;
}

xcb_generic_event_t *xcb_poll_for_event(xcb_connection_t *connection) {
    static xcb_generic_event_t *(*next)(xcb_connection_t *);
    static xcb_generic_event_t *held;
    static int held_once;
    if (!next) next = dlsym(RTLD_NEXT, "xcb_poll_for_event");
    if (!xim_connection) {
        xim_connection = connection;
        protocol_atom = atom(connection, "_XIM_PROTOCOL");
        connect_atom = atom(connection, "_XIM_XCONNECT");
    }
    if (connection != xim_connection) return next(connection);
    const char *gate = getenv("SNIP_IME_STARTUP_GATE");
    if (held && gate && access(gate, F_OK) == 0) {
        xcb_generic_event_t *event = held;
        held = NULL;
        fprintf(stderr, "[IME_STARTUP] released CONNECT_REPLY\n");
        return event;
    }
    xcb_generic_event_t *event = next(connection);
    if (!event || (event->response_type & 127) != XCB_CLIENT_MESSAGE)
        return event;
    xcb_client_message_event_t *message = (void *)event;
    if (message->type == connect_atom && message->format == 32) {
        server_window = message->data.data32[0];
        client_window = message->window;
    }
    if (!protocol_atom || message->type != protocol_atom || message->window != client_window)
        return event;
    if (message->format == 8 && message->data.data8[0] == 2 && gate && !held_once) {
        held = event;
        held_once = 1;
        fprintf(stderr, "[IME_STARTUP] held CONNECT_REPLY\n");
        return NULL;
    }
    if (message->format == 8 && message->data.data8[0] == 20) {
        fprintf(stderr, "[IME_STARTUP] XIM_ERROR ");
        for (int i = 0; i < 20; i++) fprintf(stderr, "%02x", message->data.data8[i]);
        fputc('\n', stderr);
    }
    return event;
}

uint64_t xcb_send_request64(xcb_connection_t *connection, int flags,
                          struct iovec *vectors, const xcb_protocol_request_t *request) {
    static uint64_t (*next)(xcb_connection_t *, int, struct iovec *, const xcb_protocol_request_t *);
    if (!next) next = dlsym(RTLD_NEXT, "xcb_send_request64");
    unsigned char bytes[44] = {0};
    size_t size = 0;
    for (size_t i = 0; i < request->count && size < sizeof bytes; i++) {
        size_t count = vectors[i].iov_len;
        if (count > sizeof bytes - size) count = sizeof bytes - size;
        if (vectors[i].iov_base) memcpy(bytes + size, vectors[i].iov_base, count);
        size += count;
    }
    /* X11 SendEvent(format=8) or ChangeProperty(APPEND): XIM bytes at offset 24. */
    uint32_t type = 0, target = 0;
    memcpy(&type, bytes + 20, sizeof type);
    memcpy(&target, bytes + 4, sizeof target);
    if (connection == xim_connection && protocol_atom &&
        ((size == 44 && bytes[0] == 25 && bytes[13] == 8 && type == protocol_atom) ||
         (size >= 32 && bytes[0] == 18 && bytes[1] == 2 && server_window && target == server_window))) {
        unsigned char *xim = bytes + 24;
        unsigned im = xim[4] | (xim[5] << 8), ic = xim[6] | (xim[7] << 8);
        if ((xim[0] == 50 && !im) || ((xim[0] == 54 || xim[0] == 64) && (!im || !ic)))
            fprintf(stderr, "[IME_STARTUP] premature IC opcode=%u im=%u ic=%u\n", xim[0], im, ic);
    }
    return next(connection, flags, vectors, request);
}
