/*
 *    main.cpp  --  AIS Decoder
 *
 *    Copyright (C) 2013
 *      Astra Paging Ltd / AISHub (info@aishub.net)
 *
 *    AISDecoder is free software; you can redistribute it and/or modify
 *    it under the terms of the GNU General Public License as published by
 *    the Free Software Foundation; either version 2 of the License, or
 *    (at your option) any later version.
 *
 *    AISDecoder uses parts of GNUAIS project (http://gnuais.sourceforge.net/)
 *
 */
/* This is a stripped down version for use with rtl_ais*/

/* --- Ferrite library wrap -----------------------------------------------
 *
 * In-tree, this file's networking and TCP-listener paths are unused
 * (we don't ship a UDP / TCP NMEA bridge — decoded AIVDM lines flow
 * out through the per-thread capture ring in shim/ais_shim.c).
 * The two functional changes:
 *   - `<netdb.h>`, `<sys/socket.h>`, and `tcp_listener.h` includes
 *     stripped; `send_nmea` and `initSocket` bodies wrapped in `#if 0`.
 *   - `init_ais_decoder` skips the `initSocket` / `add_nmea_ais_message`
 *     call sites; everything that touches `sock` / `addr` / `use_tcp`
 *     is excised.
 * `append_message` + `aisdecoder_next_message` form the message queue
 * the wrapper drains, and stay verbatim. `nmea_sentence_received` is
 * the receiver-side callback that calls `append_message`; that path is
 * preserved.
 */
#include <string.h>
#include <stdio.h>
#include <stdlib.h>
#include <errno.h>
#include <pthread.h>
#include "sounddecoder.h"
#include "lib/callbacks.h"

#define MAX_BUFFER_LENGTH 2048
//#define MAX_BUFFER_LENGTH 8190

static char buffer[MAX_BUFFER_LENGTH];
static unsigned int buffer_count=0;
static int debug_nmea;
/* `sock`, `addr`, and `use_tcp` were the UDP / TCP NMEA bridge state.
 * Excised — Ferrite drains AIVDM lines through `aisdecoder_next_message`
 * into the per-thread capture ring; no sockets. */
// messages can be retrived from a different thread
static pthread_mutex_t message_mutex;

// queue of decoded ais messages
struct ais_message {
    char *buffer;
    struct ais_message *next;
} *ais_messages_head, *ais_messages_tail, *last_message;

static void append_message(const char *buffer)
{
    struct ais_message *m = malloc(sizeof *m);

    m->buffer = strdup(buffer);
    m->next = NULL;
    pthread_mutex_lock(&message_mutex);

    // enqueue
    if(!ais_messages_head)
        ais_messages_head = m;
    else
        ais_messages_tail->next = m;
    ais_messages_tail = m;
    pthread_mutex_unlock(&message_mutex);
}

static void free_message(struct ais_message *m)
{
    if(m) {
        free(m->buffer);
        free(m);
    }
}

const char *aisdecoder_next_message()
{
    free_message(last_message);
    last_message = NULL;

    pthread_mutex_lock(&message_mutex);
    if(!ais_messages_head) {
        pthread_mutex_unlock(&message_mutex);
        return NULL;
    }

    // dequeue
    last_message = ais_messages_head;
    ais_messages_head = ais_messages_head->next;
    
    pthread_mutex_unlock(&message_mutex);
    return last_message->buffer;
}

/* Ferrite-lib: `initSocket`, `send_nmea`, and the broadcast-address /
 * winsock helpers below are wrapped in `#if 0` because their headers
 * (`<netdb.h>`, `<sys/socket.h>`, the TCP listener) are no longer
 * included. The single live use of `send_nmea` (inside
 * `nmea_sentence_received`) is replaced with a no-op since the
 * decoded line is already delivered via `append_message` →
 * `aisdecoder_next_message`. */
#define send_nmea(sentence, length) (0)

void sound_level_changed(float level, int channel, unsigned char high) {
    if (high != 0)
        fprintf(stderr, "Level on ch %d too high: %.0f %%\n", channel, level);
    else
        fprintf(stderr, "Level on ch %d: %.0f %%\n", channel, level);
}

void nmea_sentence_received(const char *sentence,
                          unsigned int length,
                          unsigned char sentences,
                          unsigned char sentencenum) {
    append_message(sentence);

    if (sentences == 1) {
        if (send_nmea( sentence, length) == -1){
			fprintf(stderr,"Error sending UDP packet with NMEA message: %s\n", strerror(errno));
			abort();
		}
        if (debug_nmea) fprintf(stderr, "%s", sentence);
    } else {
        if (buffer_count + length < MAX_BUFFER_LENGTH) {
            memcpy(&buffer[buffer_count], sentence, length);
            buffer_count += length;
        } else {
            buffer_count=0;
        }

        if (sentences == sentencenum && buffer_count > 0) {
            if (send_nmea( buffer, buffer_count) == -1){
				fprintf(stderr,"Error sending UDP packet with NMEA message (buffer_count=%d):%s\n",buffer_count, strerror(errno));
				abort();
			}
            if (debug_nmea) fprintf(stderr, "%s", buffer);
            buffer_count=0;
        };
    }
}

int init_ais_decoder(char * host, char * port ,int show_levels,int _debug_nmea,int buf_len,int time_print_stats, int use_tcp_listener, int tcp_keep_ais_time, int add_sample_num){
	(void)host; (void)port; (void)use_tcp_listener; (void)tcp_keep_ais_time;
	debug_nmea=_debug_nmea;
	pthread_mutex_init(&message_mutex, NULL);
    if (show_levels) on_sound_level_changed=sound_level_changed;
    on_nmea_sentence_received=nmea_sentence_received;
	initSoundDecoder(buf_len,time_print_stats,add_sample_num);
	return 0;
}

void run_rtlais_decoder(short * buff, int len)
{
	run_mem_decoder(buff,len,MAX_BUFFER_LENGTH);
}
int free_ais_decoder(void)
{
    pthread_mutex_destroy(&message_mutex);

    // free all stored messa ages
    free_message(last_message);
    last_message = NULL;
   
    while(ais_messages_head) {
        struct ais_message *m = ais_messages_head;
        ais_messages_head = ais_messages_head->next;

        free_message(m);
    }
    
    freeSoundDecoder();
    return 0;
}



/* Ferrite-lib: `isBroadcastAddress` and `initSocket` are the UDP NMEA
 * bridge — wrapped in `#if 0` because their headers (`<netdb.h>`,
 * `<sys/socket.h>`, the WSAStartup winsock setup) are no longer
 * included. Kept inline so future upstream-syncs still diff cleanly. */
#if 0
int isBroadcastAddress (const char *ipAddress) {
    // Find the last dot in the IP address
    const char *lastDot = strrchr(ipAddress, '.');
    if (lastDot != NULL) {
        // Extract the last octet after the dot
        const char *lastOctet = lastDot + 1;
        // Check if the last octet is "255"
        if (strcmp(lastOctet, "255") == 0) {
            return 1;  // Last digits are 255
        }
   }
    return 0;  //Last digits are not 255
}



int initSocket(const char *host, const char *portname) {
    struct addrinfo hints;
	int enable_broadcast=1;
    memset(&hints, 0, sizeof(hints));
    hints.ai_family=AF_UNSPEC;
    hints.ai_socktype=SOCK_DGRAM;
    hints.ai_protocol=IPPROTO_UDP;
#ifndef WIN32
    hints.ai_flags=AI_ADDRCONFIG;
#else
    int iResult = WSAStartup(MAKEWORD(2, 2), &wsaData);
    if (iResult != 0) {
        printf("WSAStartup failed: %d\n", iResult);
        return 0;
    }
#endif
    int err=getaddrinfo(host, portname, &hints, &addr);
    if (err!=0) {
        fprintf(stderr, "Failed to resolve remote socket address!\n");
#ifdef WIN32
        WSACleanup();
#endif
        return 0;
    }

    sock=socket(addr->ai_family, addr->ai_socktype, addr->ai_protocol);
    if (sock==-1) {
        fprintf(stderr, "%s",strerror(errno));
#ifdef WIN32
        WSACleanup();
#endif
        return 0;
    }
	if(isBroadcastAddress(host)){
		fprintf(stderr, "Broadcast address detected. Setting SO_BROADCAST option to socket.\n");
		  // Enable sending broadcast packets
		if (setsockopt(sock, SOL_SOCKET, SO_BROADCAST, &enable_broadcast, sizeof(enable_broadcast)) < 0) {
			perror("Failed to set socket option SO_BROADCAST:");
			exit(1);
		}
	}
	fprintf(stderr,"AIS data will be sent to %s port %s\n",host,portname);
    return 1;
}
#endif /* Ferrite-lib: end of excised UDP NMEA bridge block. */

