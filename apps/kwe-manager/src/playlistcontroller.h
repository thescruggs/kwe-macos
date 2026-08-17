// SPDX-License-Identifier: Apache-2.0
#pragma once
#include <QObject>
#include <QStringList>
#include <QHash>

class PlaylistController final : public QObject {
    Q_OBJECT
    Q_PROPERTY(QStringList names READ names NOTIFY changed)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY changed)
public:
    explicit PlaylistController(QObject *parent = nullptr);
    QStringList names() const { return m_names; }
    QString errorMessage() const { return m_error; }
    Q_INVOKABLE void create(const QString &name);
    Q_INVOKABLE void remove(const QString &name);
    Q_INVOKABLE void add(const QString &name, const QString &workshopId);
    Q_INVOKABLE void removeEntry(const QString &name, const QString &workshopId);
    Q_INVOKABLE QStringList entries(const QString &name) const { return m_entries.value(name); }
    Q_INVOKABLE bool shuffle(const QString &name) const { return m_shuffle.value(name, false); }
    Q_INVOKABLE bool repeat(const QString &name) const { return m_repeat.value(name, true); }
    Q_INVOKABLE int durationSeconds(const QString &name) const { return m_durationSeconds.value(name, 300); }
    Q_INVOKABLE QString transition(const QString &name) const { return m_transition.value(name, QStringLiteral("none")); }
    Q_INVOKABLE int transitionSeconds(const QString &name) const { return m_transitionSeconds.value(name, 0); }
    Q_INVOKABLE void setShuffle(const QString &name, bool value);
    Q_INVOKABLE void setRepeat(const QString &name, bool value);
    Q_INVOKABLE void setDurationSeconds(const QString &name, int value);
    Q_INVOKABLE void setTransition(const QString &name, const QString &value);
    Q_INVOKABLE void setTransitionSeconds(const QString &name, int value);
signals: void changed();
private:
    void load();
    void save();
    QStringList m_names;
    QHash<QString, QStringList> m_entries;
    QHash<QString, bool> m_shuffle;
    QHash<QString, bool> m_repeat;
    QHash<QString, int> m_durationSeconds;
    QHash<QString, QString> m_transition;
    QHash<QString, int> m_transitionSeconds;
    QString m_error;
};
