// SPDX-License-Identifier: GPL-3.0-or-later
// F1: the frame -> item destination rectangle per scaling mode, and the
// pointer normalisation that follows it (docs/backlog/WALLPAPER_SCALING_MODES.md).
#include <QtTest>

#include "frameitem.h"

class FrameMappingTest final : public QObject {
    Q_OBJECT

private slots:
    void aspectFitsAndCentres() {
        // 16:9 frame into a 3.55:1 item: width-limited... no — height is the
        // binding edge here (823 * 16/9 = 1463 < 2926), so it is pillarboxed.
        const QRectF r = kweFrameDestination(QSizeF(960, 540), QSizeF(2926, 823),
                                             QStringLiteral("aspect"));
        QCOMPARE(r.height(), 823.0);
        QVERIFY(qAbs(r.width() - 823.0 * 960.0 / 540.0) < 0.01);
        QVERIFY(qAbs(r.center().x() - 2926.0 / 2.0) < 0.01);
        QCOMPARE(r.top(), 0.0);
        // Unknown modes are aspect.
        QCOMPARE(kweFrameDestination(QSizeF(960, 540), QSizeF(2926, 823),
                                     QStringLiteral("bogus")), r);
    }

    void fillCoversAndCentres() {
        const QRectF r = kweFrameDestination(QSizeF(960, 540), QSizeF(2926, 823),
                                             QStringLiteral("fill"));
        // Width is the binding edge: the frame spans the item's width and
        // overflows vertically, centred.
        QCOMPARE(r.width(), 2926.0);
        QVERIFY(r.height() > 823.0);
        QVERIFY(qAbs(r.height() - 2926.0 * 540.0 / 960.0) < 0.01);
        QVERIFY(r.top() < 0.0);
        QVERIFY(qAbs(r.center().y() - 823.0 / 2.0) < 0.01);
    }

    void stretchIsTheItem() {
        QCOMPARE(kweFrameDestination(QSizeF(960, 540), QSizeF(2926, 823),
                                     QStringLiteral("stretch")),
                 QRectF(0, 0, 2926, 823));
    }

    void matchingAspectIsIdentityInEveryMode() {
        for (const char *mode : {"aspect", "fill", "stretch"}) {
            QCOMPARE(kweFrameDestination(QSizeF(1280, 720), QSizeF(2560, 1440),
                                         QString::fromLatin1(mode)),
                     QRectF(0, 0, 2560, 1440));
        }
    }

    void emptyInputsYieldNothing() {
        QVERIFY(kweFrameDestination(QSizeF(), QSizeF(100, 100), QStringLiteral("fill")).isEmpty());
        QVERIFY(kweFrameDestination(QSizeF(10, 10), QSizeF(), QStringLiteral("fill")).isEmpty());
    }

    void pointerNormalisesAgainstTheDestination() {
        // Under fill the item shows a vertical crop of the frame: the item's
        // top-left is NOT frame (0,0) but the first visible frame row. The
        // plugin normalises against the destination rectangle, so compute
        // what it would send for the item's top-left and centre.
        const QRectF r = kweFrameDestination(QSizeF(960, 540), QSizeF(2926, 823),
                                             QStringLiteral("fill"));
        const qreal yTopLeft = (0.0 - r.top()) / r.height();
        QVERIFY(yTopLeft > 0.0 && yTopLeft < 0.5);
        const qreal yCentre = (823.0 / 2.0 - r.top()) / r.height();
        QVERIFY(qAbs(yCentre - 0.5) < 1e-9);
        const qreal xRight = (2926.0 - r.left()) / r.width();
        QVERIFY(qAbs(xRight - 1.0) < 1e-9);
    }
};

QTEST_GUILESS_MAIN(FrameMappingTest)
#include "framemappingtest.moc"
