# FreeBSD port skeleton for lwdbg.
#
# Drop this in as devel/lwdbg/Makefile. It is a skeleton rather than a
# submission: DISTVERSION, MAINTAINER and the distinfo still have to be filled
# in, and CARGO_CRATES has to be generated on a FreeBSD machine (see below).
#
# Why USES=cargo rather than calling the top-level `make install`: the ports
# framework needs to know every crate in advance so it can fetch them into
# DISTDIR and build with the network off. `make install` from this repo would
# have cargo reach the network during the build phase, which ports forbids.

PORTNAME=	lwdbg
DISTVERSION=	0.1.0
CATEGORIES=	devel www
MASTER_SITES=	# USE_GITHUB

MAINTAINER=	you@example.org
COMMENT=	Measure and debug browsers through a caching proxy and a debug store
WWW=		https://github.com/peter/librewolf-debugger

LICENSE=	APACHE20 MIT
LICENSE_COMB=	dual

USES=		cargo
USE_GITHUB=	yes
GH_ACCOUNT=	peter
GH_PROJECT=	librewolf-debugger

# A workspace, so the binaries to install are named explicitly. The GPU
# backend is left off: wgpu would pull a graphics stack in for a tool whose
# CPU path is the reference implementation anyway.
CARGO_INSTALL_PATH=	cli host
CARGO_FEATURES=		--no-default-features

# Run-time only, and only for `lwdbg proxy ca-install`, which writes the local
# CA into a Gecko profile's cert9.db with certutil(1). Verified: the nss port
# ships bin/certutil.
RUN_DEPENDS=	certutil:security/nss

# Optional at run time, for `lwdbg android`.
OPTIONS_DEFINE=		ANDROID
OPTIONS_DEFAULT=	ANDROID
ANDROID_DESC=		Android debugging over adb
ANDROID_RUN_DEPENDS=	adb:devel/android-tools

PLIST_FILES=	bin/lwdbg \
		bin/lwdbg-host \
		share/man/man1/lwdbg.1.gz \
		${DATADIR_REL}/ssot/schema.sql \
		${DATADIR_REL}/extension/manifest.json \
		${DATADIR_REL}/extension/background.js \
		${DATADIR_REL}/skills/librewolf-debugger/SKILL.md

DATADIR_REL=	share/lwdbg

# The reference tree is a glob, so it goes in through a dynamic plist rather
# than being enumerated here and going stale on every doc change.
post-install:
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/lwdbg/skills/librewolf-debugger/references
	${INSTALL_DATA} ${WRKSRC}/skills/librewolf-debugger/SKILL.md \
		${STAGEDIR}${PREFIX}/share/lwdbg/skills/librewolf-debugger/
	${INSTALL_DATA} ${WRKSRC}/skills/librewolf-debugger/references/*.md \
		${STAGEDIR}${PREFIX}/share/lwdbg/skills/librewolf-debugger/references/
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/lwdbg/extension
	${INSTALL_DATA} ${WRKSRC}/extension/manifest.json \
		${WRKSRC}/extension/background.js \
		${STAGEDIR}${PREFIX}/share/lwdbg/extension/
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/lwdbg/ssot
	${INSTALL_DATA} ${WRKSRC}/ssot/schema.sql \
		${STAGEDIR}${PREFIX}/share/lwdbg/ssot/
	@${MKDIR} ${STAGEDIR}${PREFIX}/share/man/man1
	${INSTALL_MAN} ${WRKSRC}/doc/lwdbg.1 ${STAGEDIR}${PREFIX}/share/man/man1/
	@cd ${STAGEDIR}${PREFIX} && \
		${FIND} share/lwdbg/skills -type f | ${SORT} >> ${TMPPLIST}

# The binary looks for its skill tree at the path baked in at build time.
MAKE_ENV+=	LWDBG_DATADIR=${PREFIX}/share/lwdbg

.include <bsd.port.mk>

# ---------------------------------------------------------------------------
# Generating CARGO_CRATES
#
# The ports framework reads Cargo.lock and prints the list for you. On a
# FreeBSD machine, with the distfile already fetched:
#
#     make makesum
#     make cargo-crates >> Makefile
#
# Then move the emitted CARGO_CRATES block above `.include <bsd.port.mk>` and
# run `make makesum` again so distinfo covers every crate.
#
# There are roughly 250 crates in this lock file, which is why the list is not
# pasted here: it would be stale the first time a dependency moved.
# ---------------------------------------------------------------------------
