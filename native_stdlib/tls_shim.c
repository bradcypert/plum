// TLS transport boundary for Http.  This is intentionally separate from
// net_shim.c: a TLS connection is an opaque session, not a socket fd.
//
// The first implementation uses Mbed TLS' client API.  The Plum ABI sees
// only Int handles, CStrs, and byte counts; certificate loading, hostname
// verification, and the underlying socket remain native details.

#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <stdint.h>

#include <mbedtls/net_sockets.h>
#include <mbedtls/ssl.h>
#include <mbedtls/error.h>
#include <mbedtls/x509_crt.h>

#if defined(_WIN32)
#include <windows.h>
#include <wincrypt.h>
#endif

typedef struct {
    mbedtls_net_context net;
    mbedtls_ssl_context ssl;
    mbedtls_ssl_config config;
    mbedtls_x509_crt ca;
} plum_tls;

static int plum_load_ca(mbedtls_x509_crt *ca) {
#if defined(_WIN32)
    HCERTSTORE store = CertOpenSystemStoreA(0, "ROOT");
    if (store == NULL) return -1;
    int loaded = 0;
    PCCERT_CONTEXT cert = NULL;
    while ((cert = CertEnumCertificatesInStore(store, cert)) != NULL) {
        if (mbedtls_x509_crt_parse_der(ca, cert->pbCertEncoded,
                                       cert->cbCertEncoded) == 0) {
            loaded++;
        }
    }
    CertCloseStore(store, 0);
    return loaded == 0 ? -1 : 0;
#else
    static const char *paths[] = {
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/ssl/cert.pem",
        "/etc/pki/tls/certs/ca-bundle.crt",
        NULL,
    };
    for (int i = 0; paths[i] != NULL; ++i) {
        FILE *f = fopen(paths[i], "rb");
        if (f != NULL) {
            fclose(f);
            return mbedtls_x509_crt_parse_file(ca, paths[i]);
        }
    }
    return -1;
#endif
}

long long tls_connect(const char *host, long long port) {
    plum_tls *t = (plum_tls *)calloc(1, sizeof(*t));
    if (t == NULL) return -1;
    mbedtls_net_init(&t->net);
    mbedtls_ssl_init(&t->ssl);
    mbedtls_ssl_config_init(&t->config);
    mbedtls_x509_crt_init(&t->ca);

    char service[32];
    (void)snprintf(service, sizeof(service), "%lld", port);
    int rc = mbedtls_net_connect(&t->net, host, service, MBEDTLS_NET_PROTO_TCP);
    if (rc != 0 || (rc = plum_load_ca(&t->ca)) != 0 ||
        (rc = mbedtls_ssl_config_defaults(&t->config,
             MBEDTLS_SSL_IS_CLIENT, MBEDTLS_SSL_TRANSPORT_STREAM,
             MBEDTLS_SSL_PRESET_DEFAULT)) != 0) {
        goto fail;
    }
    mbedtls_ssl_conf_authmode(&t->config, MBEDTLS_SSL_VERIFY_REQUIRED);
    mbedtls_ssl_conf_ca_chain(&t->config, &t->ca, NULL);
    if ((rc = mbedtls_ssl_setup(&t->ssl, &t->config)) != 0 ||
        (rc = mbedtls_ssl_set_hostname(&t->ssl, host)) != 0) goto fail;
    mbedtls_ssl_set_bio(&t->ssl, &t->net, mbedtls_net_send,
                        mbedtls_net_recv, NULL);
    do { rc = mbedtls_ssl_handshake(&t->ssl); }
    while (rc == MBEDTLS_ERR_SSL_WANT_READ || rc == MBEDTLS_ERR_SSL_WANT_WRITE);
    if (rc != 0 || mbedtls_ssl_get_verify_result(&t->ssl) != 0) goto fail;
    return (long long)(intptr_t)t;

fail:
    mbedtls_x509_crt_free(&t->ca);
    mbedtls_ssl_config_free(&t->config);
    mbedtls_ssl_free(&t->ssl);
    mbedtls_net_free(&t->net);
    free(t);
    return -1;
}

long long tls_send(long long handle, const char *buf, long long len) {
    plum_tls *t = (plum_tls *)(intptr_t)handle;
    if (t == NULL || len < 0) return -1;
    long long done = 0;
    while (done < len) {
        int n = mbedtls_ssl_write(&t->ssl, (const unsigned char *)buf + done,
                                  (size_t)(len - done));
        if (n <= 0) return -1;
        done += n;
    }
    return done;
}

static _Thread_local unsigned char *tls_buf;
static _Thread_local size_t tls_cap;

long long tls_recv_n(long long handle, long long max_len) {
    plum_tls *t = (plum_tls *)(intptr_t)handle;
    if (t == NULL || max_len < 0) return -1;
    size_t need = (size_t)max_len + 1;
    if (tls_cap < need) {
        unsigned char *p = (unsigned char *)realloc(tls_buf, need);
        if (p == NULL) return -1;
        tls_buf = p; tls_cap = need;
    }
    int n;
    do { n = mbedtls_ssl_read(&t->ssl, tls_buf, (size_t)max_len); }
    while (n == MBEDTLS_ERR_SSL_WANT_READ || n == MBEDTLS_ERR_SSL_WANT_WRITE);
    if (n == MBEDTLS_ERR_SSL_PEER_CLOSE_NOTIFY || n == 0) return 0;
    return n < 0 ? -1 : n;
}

const char *tls_recv_data(void) { return tls_buf ? (const char *)tls_buf : ""; }

void tls_close(long long handle) {
    plum_tls *t = (plum_tls *)(intptr_t)handle;
    if (t == NULL) return;
    (void)mbedtls_ssl_close_notify(&t->ssl);
    mbedtls_x509_crt_free(&t->ca);
    mbedtls_ssl_config_free(&t->config);
    mbedtls_ssl_free(&t->ssl);
    mbedtls_net_free(&t->net);
    free(t);
}
