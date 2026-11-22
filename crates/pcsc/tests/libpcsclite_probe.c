/*
 * Client of the real libpcsclite, to check the PC/SC server of Oxydesk end to end (see ci/pcsc-server-test.sh).
 * The prototypes are declared here so that no pcsc-lite development package is needed: only libpcsclite.so.1.
 *
 * usage: probe <ready-marker-file>
 * Needs the `fake_reader` example on PCSCLITE_CSOCK_NAME. At "WAITING ..." lines it blocks in
 * SCardGetStatusChange; the script then makes the fake remove or insert the card. Every result is a line
 * "NAME value" on the standard output.
 */
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

typedef long LONG;
typedef unsigned long DWORD;
typedef long SCARDCONTEXT;
typedef long SCARDHANDLE;
typedef struct { DWORD dwProtocol; DWORD cbPciLength; } SCARD_IO_REQUEST;
typedef struct {
	const char *szReader;
	void *pvUserData;
	DWORD dwCurrentState;
	DWORD dwEventState;
	DWORD cbAtr;
	unsigned char rgbAtr[33];
} SCARD_READERSTATE;

extern const SCARD_IO_REQUEST g_rgSCardT0Pci, g_rgSCardT1Pci;
LONG SCardEstablishContext(DWORD, const void *, const void *, SCARDCONTEXT *);
LONG SCardReleaseContext(SCARDCONTEXT);
LONG SCardListReaders(SCARDCONTEXT, const char *, char *, DWORD *);
LONG SCardGetStatusChange(SCARDCONTEXT, DWORD, SCARD_READERSTATE *, DWORD);
LONG SCardConnect(SCARDCONTEXT, const char *, DWORD, DWORD, SCARDHANDLE *, DWORD *);
LONG SCardReconnect(SCARDHANDLE, DWORD, DWORD, DWORD, DWORD *);
LONG SCardDisconnect(SCARDHANDLE, DWORD);
LONG SCardBeginTransaction(SCARDHANDLE);
LONG SCardEndTransaction(SCARDHANDLE, DWORD);
LONG SCardStatus(SCARDHANDLE, char *, DWORD *, DWORD *, DWORD *, unsigned char *, DWORD *);
LONG SCardTransmit(SCARDHANDLE, const SCARD_IO_REQUEST *, const unsigned char *, DWORD,
	SCARD_IO_REQUEST *, unsigned char *, DWORD *);
LONG SCardGetAttrib(SCARDHANDLE, DWORD, unsigned char *, DWORD *);
LONG SCardCancel(SCARDCONTEXT);

#define INFINITE 0xFFFFFFFFUL
#define STATE_PRESENT 0x20
#define STATE_EMPTY 0x10
#define STATE_CHANGED 0x02

static SCARDCONTEXT ctx;

static void hex(const unsigned char *b, DWORD n)
{
	for (DWORD i = 0; i < n; i++)
		printf("%02x", b[i]);
}

static void *canceller(void *arg)
{
	(void)arg;
	usleep(300 * 1000);
	printf("CANCEL_RV %lx\n", (unsigned long)SCardCancel(ctx));
	fflush(stdout);
	return NULL;
}

int main(void)
{
	LONG rv;
	char readers[512];
	DWORD len = sizeof readers;

	rv = SCardEstablishContext(0, NULL, NULL, &ctx);
	printf("ESTABLISH %lx\n", (unsigned long)rv);
	if (rv) return 1;
	rv = SCardListReaders(ctx, NULL, readers, &len);
	printf("LIST %lx %s\n", (unsigned long)rv, rv ? "" : readers);
	if (rv) return 1;

	SCARD_READERSTATE rs[2];
	memset(rs, 0, sizeof rs);
	rs[0].szReader = readers;
	rv = SCardGetStatusChange(ctx, 1000, rs, 1);
	printf("STATE %lx %lx atr=", (unsigned long)rv, rs[0].dwEventState & 0xFFFF);
	hex(rs[0].rgbAtr, rs[0].cbAtr);
	printf(" count=%lu\n", rs[0].dwEventState >> 16);

	SCARDHANDLE card;
	DWORD proto;
	rv = SCardConnect(ctx, readers, 2 /* shared */, 3 /* T0|T1 */, &card, &proto);
	printf("CONNECT %lx proto=%lu\n", (unsigned long)rv, proto);
	if (rv) return 1;

	char name[256];
	unsigned char atr[40];
	DWORD nlen = sizeof name, state, p, alen = sizeof atr;
	rv = SCardStatus(card, name, &nlen, &state, &p, atr, &alen);
	printf("STATUS %lx name=%s state=%lx proto=%lu atr=", (unsigned long)rv, name, state & 0xFFFF, p);
	hex(atr, alen);
	printf("\n");

	unsigned char out[300];
	DWORD olen = sizeof out;
	SCARD_IO_REQUEST pci = p == 1 ? g_rgSCardT0Pci : g_rgSCardT1Pci;
	const unsigned char select[] = { 0x00, 0xA4, 0x04, 0x00 };
	rv = SCardTransmit(card, &pci, select, sizeof select, NULL, out, &olen);
	printf("TRANSMIT_SELECT %lx ", (unsigned long)rv);
	hex(out, olen);
	printf("\n");

	olen = sizeof out;
	const unsigned char read256[] = { 0x00, 0xB0, 0x00, 0x00, 0x00 };
	rv = SCardTransmit(card, &pci, read256, sizeof read256, NULL, out, &olen);
	printf("TRANSMIT_256 %lx len=%lu\n", (unsigned long)rv, olen);

	olen = 20;
	rv = SCardTransmit(card, &pci, read256, sizeof read256, NULL, out, &olen);
	printf("TRANSMIT_SMALL_BUFFER %lx need=%lu\n", (unsigned long)rv, olen);

	alen = sizeof atr;
	rv = SCardGetAttrib(card, 0x00090303, atr, &alen);
	printf("ATTRIB_ATR %lx ", (unsigned long)rv);
	hex(atr, alen);
	printf("\n");
	printf("BEGIN %lx\n", (unsigned long)SCardBeginTransaction(card));
	printf("END %lx\n", (unsigned long)SCardEndTransaction(card, 0));
	printf("RECONNECT %lx\n", (unsigned long)SCardReconnect(card, 2, 3, 0, &proto));

	/* No change since the state we know: times out. */
	rs[0].dwCurrentState = rs[0].dwEventState & ~STATE_CHANGED;
	rv = SCardGetStatusChange(ctx, 300, rs, 1);
	printf("TIMEOUT_RV %lx\n", (unsigned long)rv);

	/* SCardCancel from another thread while waiting. */
	pthread_t t;
	pthread_create(&t, NULL, canceller, NULL);
	rv = SCardGetStatusChange(ctx, INFINITE, rs, 1);
	pthread_join(t, NULL);
	printf("CANCELLED_RV %lx\n", (unsigned long)rv);

	/* Wait for the card removal that the script triggers. */
	rs[0].dwCurrentState = rs[0].dwEventState & ~STATE_CHANGED;
	printf("WAITING removal\n");
	fflush(stdout);
	rv = SCardGetStatusChange(ctx, 20000, rs, 1);
	printf("REMOVAL %lx event=%lx count=%lu\n", (unsigned long)rv, rs[0].dwEventState & 0xFFFF,
		rs[0].dwEventState >> 16);
	olen = sizeof out;
	rv = SCardTransmit(card, &pci, select, sizeof select, NULL, out, &olen);
	printf("TRANSMIT_AFTER_REMOVAL %lx\n", (unsigned long)rv);

	/* And for its return. */
	rs[0].dwCurrentState = rs[0].dwEventState & ~STATE_CHANGED;
	printf("WAITING insertion\n");
	fflush(stdout);
	rv = SCardGetStatusChange(ctx, 20000, rs, 1);
	printf("INSERTION %lx event=%lx atr=", (unsigned long)rv, rs[0].dwEventState & 0xFFFF);
	hex(rs[0].rgbAtr, rs[0].cbAtr);
	printf("\n");

	printf("DISCONNECT %lx\n", (unsigned long)SCardDisconnect(card, 0));
	printf("RELEASE %lx\n", (unsigned long)SCardReleaseContext(ctx));
	printf("DONE\n");
	return 0;
}
