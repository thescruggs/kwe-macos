// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QAbstractListModel>
#include <QJsonArray>
#include <QJsonObject>
#include <QSortFilterProxyModel>
#include <QUrl>
#include <QSet>

struct WallpaperItem {
    QString workshopId;
    QString title;
    QString kind;
    QString compatibility;
    QString detail;
    QString workshopState;
    int workshopProgress = -1;
    QUrl previewUrl;
    QString contentRoot;
    QUrl entryUrl;
    QStringList tags;
    QStringList requestedPermissions;
    int diagnosticCount = 0;
    QString diagnosticSummary;
    bool favorite = false;
};

class CatalogModel final : public QAbstractListModel {
    Q_OBJECT
    Q_PROPERTY(int totalCount READ totalCount NOTIFY statsChanged)
    Q_PROPERTY(int sceneCount READ sceneCount NOTIFY statsChanged)
    Q_PROPERTY(int videoCount READ videoCount NOTIFY statsChanged)
    Q_PROPERTY(int webCount READ webCount NOTIFY statsChanged)
    Q_PROPERTY(int issueCount READ issueCount NOTIFY statsChanged)
    Q_PROPERTY(int subscribedCount READ subscribedCount NOTIFY statsChanged)
    Q_PROPERTY(int missingCount READ missingCount NOTIFY statsChanged)
    Q_PROPERTY(int downloadingCount READ downloadingCount NOTIFY statsChanged)

public:
    enum Role {
        WorkshopIdRole = Qt::UserRole + 1,
        TitleRole,
        KindRole,
        CompatibilityRole,
        CompatibilityDetailRole,
        PreviewUrlRole,
        ContentRootRole,
        DiagnosticCountRole,
        WorkshopStateRole,
        WorkshopProgressRole,
        FavoriteRole,
        TagsRole,
        EntryUrlRole,
        DiagnosticSummaryRole,
        RequestedPermissionsRole,
    };
    Q_ENUM(Role)

    explicit CatalogModel(QObject *parent = nullptr);
    int rowCount(const QModelIndex &parent = {}) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;
    void replaceFromCatalog(const QJsonObject &catalog);
    Q_INVOKABLE void toggleFavorite(const QString &workshopId);
    bool isFavorite(const QString &workshopId) const;

    int sceneCount() const { return m_sceneCount; }
    int totalCount() const { return m_items.size(); }
    int videoCount() const { return m_videoCount; }
    int webCount() const { return m_webCount; }
    int issueCount() const { return m_issueCount; }
    int subscribedCount() const { return m_subscribedCount; }
    int missingCount() const { return m_missingCount; }
    int downloadingCount() const { return m_downloadingCount; }

signals:
    void statsChanged();
    void favoritesChanged();

private:
    QList<WallpaperItem> m_items;
    // B3: the items array of the last catalog applied. The client refreshes
    // the catalog every few seconds; an unchanged payload must not reset the
    // model (a reset invalidates every delegate and makes the grid re-settle
    // under the pointer). Only a changed payload reaches beginResetModel.
    QJsonArray m_lastItems;
    bool m_hasItems = false;
    int m_sceneCount = 0;
    int m_videoCount = 0;
    int m_webCount = 0;
    int m_issueCount = 0;
    int m_subscribedCount = 0;
    int m_missingCount = 0;
    int m_downloadingCount = 0;
    QSet<QString> m_favorites;
};

class WallpaperFilterModel final : public QSortFilterProxyModel {
    Q_OBJECT
    Q_PROPERTY(int count READ count NOTIFY countChanged)
    Q_PROPERTY(QString searchText READ searchText WRITE setSearchText NOTIFY searchTextChanged)
    Q_PROPERTY(QString kindFilter READ kindFilter WRITE setKindFilter NOTIFY kindFilterChanged)
    Q_PROPERTY(QString sortMode READ sortMode WRITE setSortMode NOTIFY sortModeChanged)
    Q_PROPERTY(bool favoritesOnly READ favoritesOnly WRITE setFavoritesOnly NOTIFY favoritesOnlyChanged)
    Q_PROPERTY(bool workshopView READ workshopView WRITE setWorkshopView NOTIFY workshopViewChanged)

public:
    explicit WallpaperFilterModel(QObject *parent = nullptr);
    QString searchText() const { return m_searchText; }
    QString kindFilter() const { return m_kindFilter; }
    int count() const { return rowCount(); }
    void setSearchText(const QString &value);
    void setKindFilter(const QString &value);
    QString sortMode() const { return m_sortMode; }
    bool favoritesOnly() const { return m_favoritesOnly; }
    bool workshopView() const { return m_workshopView; }
    void setSortMode(const QString &value);
    void setFavoritesOnly(bool value);
    void setWorkshopView(bool value);

signals:
    void searchTextChanged();
    void kindFilterChanged();
    void countChanged();
    void sortModeChanged();
    void favoritesOnlyChanged();
    void workshopViewChanged();

protected:
    bool filterAcceptsRow(int sourceRow, const QModelIndex &sourceParent) const override;

private:
    QString m_searchText;
    QString m_kindFilter = QStringLiteral("all");
    QString m_sortMode = QStringLiteral("title");
    bool m_favoritesOnly = false;
    bool m_workshopView = false;
};
