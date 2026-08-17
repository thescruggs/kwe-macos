// SPDX-License-Identifier: Apache-2.0
#include "catalogmodel.h"

#include <QJsonArray>
#include <QJsonValue>
#include <QSettings>

CatalogModel::CatalogModel(QObject *parent) : QAbstractListModel(parent) {
    QSettings settings;
    settings.beginGroup(QStringLiteral("favorites"));
    for (const auto &key : settings.childKeys()) {
        if (settings.value(key).toBool())
            m_favorites.insert(key);
    }
}

int CatalogModel::rowCount(const QModelIndex &parent) const {
    return parent.isValid() ? 0 : m_items.size();
}

QVariant CatalogModel::data(const QModelIndex &index, int role) const {
    if (!index.isValid() || index.row() < 0 || index.row() >= m_items.size()) {
        return {};
    }
    const auto &item = m_items.at(index.row());
    switch (role) {
    case WorkshopIdRole: return item.workshopId;
    case TitleRole: return item.title;
    case KindRole: return item.kind;
    case CompatibilityRole: return item.compatibility;
    case CompatibilityDetailRole: return item.detail;
    case PreviewUrlRole: return item.previewUrl;
    case ContentRootRole: return item.contentRoot;
    case DiagnosticCountRole: return item.diagnosticCount;
    case WorkshopStateRole: return item.workshopState;
    case WorkshopProgressRole: return item.workshopProgress;
    case FavoriteRole: return item.favorite;
    case TagsRole: return item.tags;
    case EntryUrlRole: return item.entryUrl;
    case DiagnosticSummaryRole: return item.diagnosticSummary;
    case RequestedPermissionsRole: return item.requestedPermissions;
    default: return {};
    }
}

QHash<int, QByteArray> CatalogModel::roleNames() const {
    return {
        {WorkshopIdRole, "workshopId"},
        {TitleRole, "title"},
        {KindRole, "kind"},
        {CompatibilityRole, "compatibility"},
        {CompatibilityDetailRole, "compatibilityDetail"},
        {PreviewUrlRole, "previewUrl"},
        {ContentRootRole, "contentRoot"},
        {DiagnosticCountRole, "diagnosticCount"},
        {WorkshopStateRole, "workshopState"},
        {WorkshopProgressRole, "workshopProgress"},
        {FavoriteRole, "favorite"},
        {TagsRole, "tags"},
        {EntryUrlRole, "entryUrl"},
        {DiagnosticSummaryRole, "diagnosticSummary"},
        {RequestedPermissionsRole, "requestedPermissions"},
    };
}

void CatalogModel::replaceFromCatalog(const QJsonObject &catalog) {
    QList<WallpaperItem> replacement;
    int scene = 0;
    int video = 0;
    int web = 0;
    int issues = 0;
    int subscribed = 0;
    int missing = 0;
    int downloading = 0;
    const auto values = catalog.value(QStringLiteral("items")).toArray();
    replacement.reserve(values.size());
    for (const auto &value : values) {
        const auto object = value.toObject();
        WallpaperItem item;
        item.workshopId = object.value(QStringLiteral("workshop_id")).toString();
        item.title = object.value(QStringLiteral("title")).toString();
        item.kind = object.value(QStringLiteral("kind")).toString();
        item.compatibility = object.value(QStringLiteral("compatibility")).toString();
        item.detail = object.value(QStringLiteral("compatibility_detail")).toString();
        item.workshopState = object.value(QStringLiteral("workshop_state")).toString();
        item.workshopProgress = object.value(QStringLiteral("workshop_progress")).toInt(-1);
        item.contentRoot = object.value(QStringLiteral("content_root")).toString();
        const auto entry = object.value(QStringLiteral("entry_file")).toString();
        if (!entry.isEmpty()) item.entryUrl = QUrl::fromLocalFile(entry);
        for (const auto &tag : object.value(QStringLiteral("tags")).toArray())
            item.tags.push_back(tag.toString());
        for (const auto &permission : object.value(QStringLiteral("requested_permissions")).toArray())
            item.requestedPermissions.push_back(permission.toString());
        item.diagnosticCount = object.value(QStringLiteral("diagnostics")).toArray().size();
        QStringList diagnosticMessages;
        for (const auto &diagnostic : object.value(QStringLiteral("diagnostics")).toArray()) {
            const auto message = diagnostic.toObject().value(QStringLiteral("message")).toString();
            if (!message.isEmpty()) diagnosticMessages.push_back(message);
            if (diagnosticMessages.size() == 3) break;
        }
        item.diagnosticSummary = diagnosticMessages.join(QStringLiteral(" "));
        item.favorite = m_favorites.contains(item.workshopId);
        const auto preview = object.value(QStringLiteral("preview_file")).toString();
        if (!preview.isEmpty()) item.previewUrl = QUrl::fromLocalFile(preview);
        if (item.kind == QStringLiteral("scene")) ++scene;
        else if (item.kind == QStringLiteral("video")) ++video;
        else if (item.kind == QStringLiteral("web")) ++web;
        if (item.diagnosticCount > 0 || item.kind == QStringLiteral("invalid") || item.kind == QStringLiteral("unknown")) ++issues;
        if (item.workshopState == QStringLiteral("subscribed_installed")) ++subscribed;
        if (item.workshopState == QStringLiteral("subscribed_missing")) ++missing;
        if (item.workshopState == QStringLiteral("downloading")) ++downloading;
        replacement.push_back(std::move(item));
    }
    beginResetModel();
    m_items = std::move(replacement);
    m_sceneCount = scene;
    m_videoCount = video;
    m_webCount = web;
    m_issueCount = issues;
    m_subscribedCount = subscribed;
    m_missingCount = missing;
    m_downloadingCount = downloading;
    endResetModel();
    emit statsChanged();
}

bool CatalogModel::isFavorite(const QString &workshopId) const {
    return m_favorites.contains(workshopId);
}

void CatalogModel::toggleFavorite(const QString &workshopId) {
    if (workshopId.isEmpty()) return;
    if (m_favorites.contains(workshopId)) m_favorites.remove(workshopId);
    else m_favorites.insert(workshopId);
    QSettings settings;
    settings.beginGroup(QStringLiteral("favorites"));
    settings.setValue(workshopId, m_favorites.contains(workshopId));
    settings.endGroup();
    for (int row = 0; row < m_items.size(); ++row) {
        if (m_items.at(row).workshopId == workshopId) {
            m_items[row].favorite = m_favorites.contains(workshopId);
            emit dataChanged(index(row), index(row), {FavoriteRole});
            break;
        }
    }
    emit favoritesChanged();
}

bool CatalogModel::isPermissionGranted(const QString &workshopId, const QString &permission) const {
    QSettings settings;
    return settings.value(QStringLiteral("permissions/%1/%2").arg(workshopId, permission), false).toBool();
}

void CatalogModel::togglePermission(const QString &workshopId, const QString &permission) {
    if (workshopId.isEmpty() || !QStringList{QStringLiteral("network"), QStringLiteral("pointer"), QStringLiteral("audio")}.contains(permission)) return;
    QSettings settings;
    const auto key = QStringLiteral("permissions/%1/%2").arg(workshopId, permission);
    settings.setValue(key, !settings.value(key, false).toBool());
    emit favoritesChanged();
}

WallpaperFilterModel::WallpaperFilterModel(QObject *parent) : QSortFilterProxyModel(parent) {
    setDynamicSortFilter(true);
    setSortCaseSensitivity(Qt::CaseInsensitive);
    connect(this, &QAbstractItemModel::rowsInserted, this, &WallpaperFilterModel::countChanged);
    connect(this, &QAbstractItemModel::rowsRemoved, this, &WallpaperFilterModel::countChanged);
    connect(this, &QAbstractItemModel::modelReset, this, &WallpaperFilterModel::countChanged);
}

void WallpaperFilterModel::setSearchText(const QString &value) {
    if (m_searchText == value) return;
    beginFilterChange();
    m_searchText = value;
    endFilterChange(Direction::Rows);
    emit searchTextChanged();
}

void WallpaperFilterModel::setKindFilter(const QString &value) {
    if (m_kindFilter == value) return;
    beginFilterChange();
    m_kindFilter = value;
    endFilterChange(Direction::Rows);
    emit kindFilterChanged();
}

void WallpaperFilterModel::setSortMode(const QString &value) {
    if (m_sortMode == value) return;
    m_sortMode = value;
    if (m_sortMode == QStringLiteral("kind")) sort(CatalogModel::KindRole, Qt::AscendingOrder);
    else sort(CatalogModel::TitleRole, Qt::AscendingOrder);
    emit sortModeChanged();
}

void WallpaperFilterModel::setFavoritesOnly(bool value) {
    if (m_favoritesOnly == value) return;
    beginFilterChange();
    m_favoritesOnly = value;
    endFilterChange(Direction::Rows);
    emit favoritesOnlyChanged();
}

bool WallpaperFilterModel::filterAcceptsRow(int sourceRow, const QModelIndex &sourceParent) const {
    const auto index = sourceModel()->index(sourceRow, 0, sourceParent);
    const auto title = sourceModel()->data(index, CatalogModel::TitleRole).toString();
    const auto id = sourceModel()->data(index, CatalogModel::WorkshopIdRole).toString();
    const auto kind = sourceModel()->data(index, CatalogModel::KindRole).toString();
    const bool favorite = sourceModel()->data(index, CatalogModel::FavoriteRole).toBool();
    const bool kindMatches = m_kindFilter == QStringLiteral("all") || kind == m_kindFilter;
    const bool favoriteMatches = !m_favoritesOnly || favorite;
    const bool searchMatches = m_searchText.trimmed().isEmpty()
        || title.contains(m_searchText.trimmed(), Qt::CaseInsensitive)
        || id.contains(m_searchText.trimmed(), Qt::CaseInsensitive);
    return kindMatches && favoriteMatches && searchMatches;
}
