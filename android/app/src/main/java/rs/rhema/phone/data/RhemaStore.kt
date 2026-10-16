package rs.rhema.phone.data

import android.content.Context
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.preferencesDataStore

// One DataStore file holds everything the app persists; the two repositories
// in this package are views over it.
val Context.rhemaStore: DataStore<Preferences> by preferencesDataStore(name = "rhema")
