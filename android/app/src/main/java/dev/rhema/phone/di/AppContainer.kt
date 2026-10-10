package dev.rhema.phone.di

import android.content.Context
import dev.rhema.phone.data.SettingsRepository
import dev.rhema.phone.data.StatusRepository
import dev.rhema.phone.data.rhemaStore
import dev.rhema.phone.net.ClawApi
import dev.rhema.phone.notif.Notifier
import dev.rhema.phone.work.schedulePeriodicWork
import java.util.concurrent.TimeUnit
import kotlinx.serialization.json.Json
import okhttp3.OkHttpClient

// Manual DI: one object graph, built once in Application.onCreate, passed
// down by hand. No framework on purpose.
class AppContainer(context: Context) {
    private val appContext: Context = context.applicationContext

    val json: Json = Json { ignoreUnknownKeys = true }

    val settings = SettingsRepository(appContext.rhemaStore)
    val status = StatusRepository(appContext.rhemaStore, json)

    // readTimeout is zeroed only for the SSE client; regular calls keep a
    // timeout so a wedged claw cannot hang a POST forever.
    private val http: OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(10, TimeUnit.SECONDS)
        .writeTimeout(10, TimeUnit.SECONDS)
        .readTimeout(30, TimeUnit.SECONDS)
        .build()

    val api = ClawApi(settings, http, json)

    val notifier = Notifier(appContext)

    fun schedulePeriodicWork() = schedulePeriodicWork(appContext)
}
